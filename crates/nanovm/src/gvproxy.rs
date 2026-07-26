use std::{
    fs,
    io::{self, Read, Write},
    net::SocketAddr,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde::Serialize;
use thiserror::Error;

const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);
const API_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum GvproxyError {
    #[error("gvproxy exited before creating its sockets: {0}")]
    EarlyExit(std::process::ExitStatus),

    #[error("gvproxy did not create {path} within {timeout:?}")]
    SocketTimeout { path: PathBuf, timeout: Duration },

    #[error("refusing to expose a VM port on non-loopback host address {0}")]
    NonLoopbackForward(SocketAddr),

    #[error("host port zero cannot identify the resulting gvproxy listener")]
    UnspecifiedHostPort,

    #[error("gvproxy services API returned {status}: {body}")]
    Api { status: String, body: String },

    #[error("gvproxy services API returned an invalid HTTP response")]
    InvalidApiResponse,

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Io(#[from] io::Error),
}

/// One owned gvproxy process supplying a private network stack to one VM.
///
/// The caller supplies an exclusive state directory. The process creates its
/// vfkit-compatible unixgram socket and private services socket there. Dropping
/// this value terminates and reaps gvproxy.
pub struct Gvproxy {
    child: Child,
    network_socket: PathBuf,
    services_socket: PathBuf,
}

impl Gvproxy {
    /// Starts gvproxy and waits for both of its local sockets to become ready.
    ///
    /// # Errors
    ///
    /// Returns an error when the state directory or log cannot be prepared,
    /// gvproxy cannot start, or its network and services sockets do not become
    /// ready before the startup deadline.
    pub fn spawn(binary: &Path, state_directory: &Path, log: &Path) -> Result<Self, GvproxyError> {
        fs::create_dir_all(state_directory)?;
        if let Some(parent) = log.parent() {
            fs::create_dir_all(parent)?;
        }
        let network_socket = state_directory.join("network.sock");
        let services_socket = state_directory.join("services.sock");
        remove_stale_socket(&network_socket)?;
        remove_stale_socket(&services_socket)?;

        let log = fs::File::create(log)?;
        let mut child = std::process::Command::new(binary)
            .arg("--listen-vfkit")
            .arg(format!("unixgram:{}", network_socket.display()))
            .arg("--services")
            .arg(format!("unix://{}", services_socket.display()))
            .arg("--ssh-port")
            .arg("-1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()?;

        let started_at = Instant::now();
        while !network_socket.exists() || !services_socket.exists() {
            if let Some(status) = child.try_wait()? {
                return Err(GvproxyError::EarlyExit(status));
            }
            if started_at.elapsed() >= SOCKET_TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                return Err(GvproxyError::SocketTimeout {
                    path: if network_socket.exists() {
                        services_socket
                    } else {
                        network_socket
                    },
                    timeout: SOCKET_TIMEOUT,
                });
            }
            thread::sleep(Duration::from_millis(10));
        }

        Ok(Self {
            child,
            network_socket,
            services_socket,
        })
    }

    #[must_use]
    pub fn network_socket(&self) -> &Path {
        &self.network_socket
    }

    /// Forwards one loopback-only host TCP listener to a guest TCP listener.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-loopback or unspecified host endpoint, a
    /// services-socket failure, or a rejected gvproxy request.
    pub fn forward_tcp(&self, local: SocketAddr, remote: SocketAddr) -> Result<(), GvproxyError> {
        if !local.ip().is_loopback() {
            return Err(GvproxyError::NonLoopbackForward(local));
        }
        if local.port() == 0 {
            return Err(GvproxyError::UnspecifiedHostPort);
        }
        let body = serde_json::to_vec(&ExposeRequest {
            local,
            remote,
            protocol: "tcp",
        })?;
        services_request(&self.services_socket, "/services/forwarder/expose", &body)
    }

    /// Removes a previously configured loopback TCP forward.
    ///
    /// # Errors
    ///
    /// Returns an error when the services socket cannot be reached or gvproxy
    /// rejects the request.
    pub fn unforward_tcp(&self, local: SocketAddr) -> Result<(), GvproxyError> {
        let body = serde_json::to_vec(&UnexposeRequest {
            local,
            protocol: "tcp",
        })?;
        services_request(&self.services_socket, "/services/forwarder/unexpose", &body)
    }
}

impl Drop for Gvproxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Serialize)]
struct ExposeRequest {
    local: SocketAddr,
    remote: SocketAddr,
    protocol: &'static str,
}

#[derive(Serialize)]
struct UnexposeRequest {
    local: SocketAddr,
    protocol: &'static str,
}

fn remove_stale_socket(path: &Path) -> Result<(), io::Error> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn services_request(socket: &Path, path: &str, body: &[u8]) -> Result<(), GvproxyError> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(API_TIMEOUT))?;
    stream.set_write_timeout(Some(API_TIMEOUT))?;
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()?;

    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let response = String::from_utf8_lossy(&response);
    let (head, body) = response
        .split_once("\r\n\r\n")
        .ok_or(GvproxyError::InvalidApiResponse)?;
    let status = head
        .lines()
        .next()
        .ok_or(GvproxyError::InvalidApiResponse)?;
    if !status.starts_with("HTTP/1.1 200 ") && !status.starts_with("HTTP/1.0 200 ") {
        return Err(GvproxyError::Api {
            status: status.to_owned(),
            body: body.trim().to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddrV4};

    use super::*;

    #[test]
    fn refuses_non_loopback_forwards_before_contacting_gvproxy() {
        let proxy = Gvproxy {
            child: std::process::Command::new("/usr/bin/true").spawn().unwrap(),
            network_socket: PathBuf::new(),
            services_socket: PathBuf::new(),
        };
        let local = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 9222));
        let remote = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 127, 2), 9222));

        assert!(matches!(
            proxy.forward_tcp(local, remote),
            Err(GvproxyError::NonLoopbackForward(address)) if address == local
        ));
    }
}

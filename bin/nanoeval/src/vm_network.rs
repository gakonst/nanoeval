use std::{
    env, fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Stdio},
    thread,
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};
use tempfile::TempDir;
use thiserror::Error;
use tokio::process::Command;

const GVPROXY_VERSION: &str = "v0.8.9";
const GVPROXY_SOCKET_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub(crate) enum GvproxyError {
    #[error("NANOEVAL_GVPROXY does not name a file: {0}")]
    InvalidOverride(PathBuf),

    #[error("gvproxy is not published for {os}/{architecture}")]
    UnsupportedPlatform {
        os: &'static str,
        architecture: &'static str,
    },

    #[error("failed to download gvproxy: curl exited with {0}")]
    Download(std::process::ExitStatus),

    #[error("downloaded gvproxy digest was {actual}, expected {expected}")]
    Digest {
        expected: &'static str,
        actual: String,
    },

    #[error("gvproxy exited before creating its network socket: {0}")]
    EarlyExit(std::process::ExitStatus),

    #[error("gvproxy did not create {path} within {timeout:?}")]
    SocketTimeout { path: PathBuf, timeout: Duration },

    #[error(transparent)]
    Io(#[from] io::Error),
}

/// One userspace network stack dedicated to one VM attempt.
pub(crate) struct Gvproxy {
    child: Child,
    _directory: TempDir,
    socket: PathBuf,
}

impl Gvproxy {
    pub(crate) fn spawn(binary: &Path, log: &Path) -> Result<Self, GvproxyError> {
        if let Some(parent) = log.parent() {
            fs::create_dir_all(parent)?;
        }
        let directory = tempfile::Builder::new()
            .prefix("nanoeval-gvproxy-")
            .tempdir()?;
        let socket = directory.path().join("network.sock");
        let log = fs::File::create(log)?;
        let mut child = std::process::Command::new(binary)
            .arg("--listen-vfkit")
            .arg(format!("unixgram:{}", socket.display()))
            .arg("--ssh-port")
            .arg("-1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()?;
        let started_at = Instant::now();
        while !socket.exists() {
            if let Some(status) = child.try_wait()? {
                return Err(GvproxyError::EarlyExit(status));
            }
            if started_at.elapsed() >= GVPROXY_SOCKET_TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                return Err(GvproxyError::SocketTimeout {
                    path: socket,
                    timeout: GVPROXY_SOCKET_TIMEOUT,
                });
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok(Self {
            child,
            _directory: directory,
            socket,
        })
    }

    pub(crate) fn socket(&self) -> &Path {
        &self.socket
    }
}

impl Drop for Gvproxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(crate) async fn prepare_gvproxy(cache: &Path) -> Result<PathBuf, GvproxyError> {
    if let Some(path) = env::var_os("NANOEVAL_GVPROXY").map(PathBuf::from) {
        return path
            .is_file()
            .then_some(path.clone())
            .ok_or(GvproxyError::InvalidOverride(path));
    }
    if let Some(path) = find_on_path("gvproxy") {
        return Ok(path);
    }
    let artifact = gvproxy_artifact()?;
    let directory = cache.join("gvproxy").join(GVPROXY_VERSION);
    let binary = directory.join("gvproxy");
    if binary.is_file() && file_digest(&binary)? == artifact.digest {
        return Ok(binary);
    }
    fs::create_dir_all(&directory)?;
    let temporary = directory.join(format!("gvproxy.{}.tmp", std::process::id()));
    let status = Command::new("/usr/bin/curl")
        .arg("--fail")
        .arg("--location")
        .arg("--silent")
        .arg("--show-error")
        .arg("--output")
        .arg(&temporary)
        .arg(artifact.url)
        .status()
        .await?;
    if !status.success() {
        return Err(GvproxyError::Download(status));
    }
    let actual = file_digest(&temporary)?;
    if actual != artifact.digest {
        let _ = fs::remove_file(&temporary);
        return Err(GvproxyError::Digest {
            expected: artifact.digest,
            actual,
        });
    }
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o755))?;
    fs::rename(temporary, &binary)?;
    Ok(binary)
}

struct GvproxyArtifact {
    url: &'static str,
    digest: &'static str,
}

fn gvproxy_artifact() -> Result<GvproxyArtifact, GvproxyError> {
    let artifact = match (env::consts::OS, env::consts::ARCH) {
        ("macos", "aarch64" | "x86_64") => GvproxyArtifact {
            url: "https://github.com/containers/gvisor-tap-vsock/releases/download/v0.8.9/gvproxy-darwin",
            digest: "c6f7b4bc7f21bf810b5cf54e04d979b014c5d96472a03a9e97fe62a00940067c",
        },
        ("linux", "aarch64") => GvproxyArtifact {
            url: "https://github.com/containers/gvisor-tap-vsock/releases/download/v0.8.9/gvproxy-linux-arm64",
            digest: "6ecca02839254c9a0cc184bba7aac63755a22d7ed10d455b852528a99d7f7d4b",
        },
        ("linux", "x86_64") => GvproxyArtifact {
            url: "https://github.com/containers/gvisor-tap-vsock/releases/download/v0.8.9/gvproxy-linux-amd64",
            digest: "3011c5629c9138d2050fb23c510e09ae53e30ec52e6a9ab85632bc1550e8ef63",
        },
        (os, architecture) => {
            return Err(GvproxyError::UnsupportedPlatform { os, architecture });
        }
    };
    Ok(artifact)
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    env::var_os("PATH")
        .into_iter()
        .flat_map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
}

fn file_digest(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    io::copy(&mut file, &mut digest)?;
    Ok(format!("{:x}", digest.finalize()))
}

//! A headed Chromium process isolated inside one libkrun microVM.
//!
//! The host owns the VMM and private loopback CDP forward. Every spawn reflinks
//! an immutable ext4 template into a disposable attempt disk; no Docker daemon,
//! host browser window, shared profile, or host network listener is involved.

use std::{
    ffi::OsString,
    fs,
    io::{self, Read, Seek, SeekFrom},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use nanovm::{
    EgressLease, GuestCommand, Gvproxy, Network, PrivateVmProcessConfig, SharedDirectory, VmConfig,
    VmProcessConfig,
};
use serde::Deserialize;
use tempfile::TempDir;
use thiserror::Error;
use tokio::{process::Child, time};
use url::Url;

const GUEST_ADDRESS: Ipv4Addr = Ipv4Addr::new(192, 168, 127, 2);
const GUEST_CDP_RELAY_PORT: u16 = 9_223;
const DEFAULT_CPUS: u8 = 2;
const DEFAULT_MEMORY_MIB: u32 = 2_048;
const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_ERROR_LOG_BYTES: u64 = 64 * 1_024;
const BROWSER_SCRIPT: &str = concat!(
    "set -eu; ",
    "while [ \"$#\" -gt 0 ]; do ",
    "tag=$1; guest=$2; shift 2; ",
    "mkdir -p -- \"$guest\"; ",
    "mount -t virtiofs -o ro \"$tag\" \"$guest\"; ",
    "done; ",
    "rm -f /etc/resolv.conf; ",
    "printf 'nameserver 192.168.127.1\\n' > /etc/resolv.conf; ",
    "mkdir -p /tmp/.X11-unix /home/browser/profile; ",
    "chown -R browser:browser /home/browser; ",
    "Xvfb :99 -screen 0 1920x1080x24 -nolisten tcp -ac >/var/log/xvfb.log 2>&1 & ",
    "socat TCP-LISTEN:9223,bind=0.0.0.0,reuseaddr,fork TCP:127.0.0.1:9222 ",
    ">/var/log/cdp-relay.log 2>&1 & ",
    "exec su browser -s /bin/sh -c '",
    "DISPLAY=:99 exec chromium-browser ",
    "--disable-dev-shm-usage ",
    "--disable-features=TranslateUI ",
    "--no-default-browser-check ",
    "--no-first-run ",
    "--remote-debugging-address=0.0.0.0 ",
    "--remote-debugging-port=9222 ",
    "--user-data-dir=/home/browser/profile ",
    "--window-size=1920,1080 ",
    "about:blank'",
);

#[derive(Debug, Error)]
pub enum BrowserVmError {
    #[error("browser VM configuration is invalid: {0}")]
    InvalidConfig(&'static str),

    #[error("browser VM root disk is not a file: {0}")]
    InvalidRootDisk(PathBuf),

    #[error("browser VM VMM executable is not a file: {0}")]
    InvalidVmm(PathBuf),

    #[error("browser VM gvproxy executable is not a file: {0}")]
    InvalidGvproxy(PathBuf),

    #[error("browser VM firmware directory is not a directory: {0}")]
    InvalidFirmwareDirectory(PathBuf),

    #[error("browser VM egress must request internet access")]
    InvalidEgressNetwork,

    #[error("browser VM egress host mount is not a directory: {0}")]
    InvalidEgressMount(PathBuf),

    #[error("browser VM egress guest mount must be absolute: {0}")]
    InvalidGuestMount(PathBuf),

    #[error("failed to create the browser VM's private root disk: {0}")]
    RootDisk(io::Error),

    #[error("failed to reserve a private host CDP port: {0}")]
    ReservePort(io::Error),

    #[error("failed to spawn the browser VMM: {0}")]
    Spawn(io::Error),

    #[error("failed to inspect the browser VMM: {0}")]
    InspectVmm(io::Error),

    #[error("browser VMM exited before CDP became ready: {status}\n{log}")]
    EarlyExit {
        status: std::process::ExitStatus,
        log: String,
    },

    #[error("browser CDP endpoint {endpoint} was not ready within {timeout:?}\n{log}")]
    StartupTimeout {
        endpoint: Box<Url>,
        timeout: Duration,
        log: String,
    },

    #[error("failed to construct the browser CDP endpoint: {0}")]
    Endpoint(#[from] url::ParseError),

    #[error("browser CDP metadata was invalid: {0}")]
    CdpMetadata(#[from] serde_json::Error),

    #[error("browser returned an unusable WebSocket endpoint: {0}")]
    InvalidWebSocketEndpoint(Box<Url>),

    #[error("failed to construct the browser readiness client: {0}")]
    HttpClient(reqwest::Error),

    #[error("browser VMM did not exit within {0:?}")]
    ShutdownTimeout(Duration),

    #[error(transparent)]
    Network(#[from] nanovm::GvproxyError),

    #[error(transparent)]
    ProcessConfig(#[from] nanovm::VmProcessError),
}

/// Builder for one private headed Chromium VM.
pub struct BrowserVmBuilder {
    root_disk: PathBuf,
    vmm: PathBuf,
    gvproxy: PathBuf,
    firmware_directory: Option<PathBuf>,
    cpus: u8,
    memory_mib: u32,
    startup_timeout: Duration,
    egress: EgressLease,
}

impl BrowserVmBuilder {
    /// Creates a headed-browser VM builder from explicit runtime artifacts.
    pub fn new(
        root_disk: impl Into<PathBuf>,
        vmm: impl Into<PathBuf>,
        gvproxy: impl Into<PathBuf>,
    ) -> Self {
        Self {
            root_disk: root_disk.into(),
            vmm: vmm.into(),
            gvproxy: gvproxy.into(),
            firmware_directory: None,
            cpus: DEFAULT_CPUS,
            memory_mib: DEFAULT_MEMORY_MIB,
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            egress: EgressLease::internet(),
        }
    }

    #[must_use]
    /// Adds the directory containing `libkrunfw` runtime libraries.
    pub fn firmware_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.firmware_directory = Some(directory.into());
        self
    }

    #[must_use]
    /// Sets the guest vCPU count.
    pub fn cpus(mut self, cpus: u8) -> Self {
        self.cpus = cpus;
        self
    }

    #[must_use]
    /// Sets guest memory in mebibytes.
    pub fn memory_mib(mut self, memory_mib: u32) -> Self {
        self.memory_mib = memory_mib;
        self
    }

    #[must_use]
    /// Bounds VM boot, Chromium startup, and private CDP forwarding.
    pub fn startup_timeout(mut self, timeout: Duration) -> Self {
        self.startup_timeout = timeout;
        self
    }

    /// Supplies VM-facing proxy configuration, read-only CA mounts, and
    /// provider lifecycle guards. The browser owns its dedicated gvproxy
    /// transport, so the lease must request internet access rather than a
    /// caller-owned network socket.
    #[must_use]
    pub fn egress(mut self, egress: EgressLease) -> Self {
        self.egress = egress;
        self
    }

    /// Clones the immutable root disk and starts one private headed Chromium VM.
    ///
    /// # Errors
    ///
    /// Returns a typed configuration, filesystem, process, network, readiness,
    /// or CDP metadata error. A failed start terminates its VMM and network
    /// helper before returning.
    pub async fn spawn(self) -> Result<BrowserVm, BrowserVmError> {
        validate_file(&self.root_disk, BrowserVmError::InvalidRootDisk)?;
        validate_file(&self.vmm, BrowserVmError::InvalidVmm)?;
        validate_file(&self.gvproxy, BrowserVmError::InvalidGvproxy)?;
        if self.cpus == 0 {
            return Err(BrowserVmError::InvalidConfig("CPU count must be nonzero"));
        }
        if self.memory_mib == 0 {
            return Err(BrowserVmError::InvalidConfig("memory must be nonzero"));
        }
        if self.startup_timeout.is_zero() {
            return Err(BrowserVmError::InvalidConfig(
                "startup timeout must be nonzero",
            ));
        }
        if let Some(firmware) = &self.firmware_directory
            && !firmware.is_dir()
        {
            return Err(BrowserVmError::InvalidFirmwareDirectory(firmware.clone()));
        }
        if self.egress.network() != &Network::Internet {
            return Err(BrowserVmError::InvalidEgressNetwork);
        }
        for mount in self.egress.guest_mounts() {
            if !mount.host_path.is_dir() {
                return Err(BrowserVmError::InvalidEgressMount(mount.host_path.clone()));
            }
            if !mount.guest_path.is_absolute() {
                return Err(BrowserVmError::InvalidGuestMount(mount.guest_path.clone()));
            }
        }

        let directory = tempfile::Builder::new()
            .prefix("nanovm-browser-")
            .tempdir()
            .map_err(BrowserVmError::RootDisk)?;
        let root_disk = directory.path().join("rootfs.ext4");
        reflink_copy::reflink_or_copy(&self.root_disk, &root_disk)
            .map_err(BrowserVmError::RootDisk)?;

        let network_directory = directory.path().join("network");
        let network_log = directory.path().join("gvproxy.log");
        let network = Gvproxy::spawn(&self.gvproxy, &network_directory, &network_log)?;
        let local = reserve_loopback_address()?;
        let remote = SocketAddr::V4(SocketAddrV4::new(GUEST_ADDRESS, GUEST_CDP_RELAY_PORT));
        network.forward_tcp(local, remote)?;
        let cdp_http_endpoint = Url::parse(&format!("http://{local}"))?;

        let browser_log_path = directory.path().join("browser-vm.log");
        let browser_log = fs::File::create(&browser_log_path).map_err(BrowserVmError::Spawn)?;
        let browser_error_log = browser_log.try_clone().map_err(BrowserVmError::Spawn)?;
        let mut vm_config = VmConfig::ext4(&root_disk)
            .network(Network::gvproxy(network.network_socket()))
            .cpus(self.cpus)
            .memory_mib(self.memory_mib);
        let mut guest_command =
            GuestCommand::new("/bin/sh").args(["-c", BROWSER_SCRIPT, "nanovm-browser"]);
        for mount in self.egress.guest_mounts() {
            vm_config = vm_config
                .shared_directory(SharedDirectory::read_only(&mount.tag, &mount.host_path));
            guest_command = guest_command
                .arg(&mount.tag)
                .arg(mount.guest_path.as_os_str());
        }
        for (name, value) in self.egress.guest_environment() {
            guest_command = guest_command.env(name, value);
        }
        let process_config = VmProcessConfig::new(vm_config, guest_command).write_private()?;

        let mut command = tokio::process::Command::new(&self.vmm);
        command
            .env_clear()
            .arg("vm")
            .arg("run-config")
            .arg("--config")
            .arg(process_config.path())
            .stdin(Stdio::null())
            .stdout(Stdio::from(browser_log))
            .stderr(Stdio::from(browser_error_log))
            .kill_on_drop(true);
        if let Some(firmware) = self.firmware_directory {
            command.env("DYLD_LIBRARY_PATH", dynamic_library_path(&firmware));
        }
        let child = command.spawn().map_err(BrowserVmError::Spawn)?;
        let mut browser = BrowserVm {
            child,
            _network: network,
            cdp_endpoint: cdp_http_endpoint,
            root_disk,
            log: browser_log_path,
            _egress: self.egress,
            _process_config: process_config,
            _directory: directory,
        };
        browser.cdp_endpoint = browser
            .wait_until_ready(self.startup_timeout, local)
            .await?;
        Ok(browser)
    }
}

/// One live headed Chromium process isolated inside a libkrun microVM.
pub struct BrowserVm {
    child: Child,
    _network: Gvproxy,
    cdp_endpoint: Url,
    root_disk: PathBuf,
    log: PathBuf,
    _egress: EgressLease,
    _process_config: PrivateVmProcessConfig,
    _directory: TempDir,
}

impl BrowserVm {
    #[must_use]
    /// Returns the private loopback WebSocket endpoint for this browser.
    pub fn cdp_endpoint(&self) -> &Url {
        &self.cdp_endpoint
    }

    #[must_use]
    /// Returns the attempt-private copy-on-write root disk.
    pub fn root_disk(&self) -> &Path {
        &self.root_disk
    }

    /// Terminates and reaps the VMM, network helper, and temporary disk.
    ///
    /// # Errors
    ///
    /// Returns an error when the VMM cannot be inspected, terminated, or
    /// reaped within the shutdown deadline.
    pub async fn shutdown(mut self) -> Result<(), BrowserVmError> {
        if self
            .child
            .try_wait()
            .map_err(BrowserVmError::InspectVmm)?
            .is_some()
        {
            return Ok(());
        }
        self.child.start_kill().map_err(BrowserVmError::Spawn)?;
        match time::timeout(SHUTDOWN_TIMEOUT, self.child.wait()).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error)) => Err(BrowserVmError::InspectVmm(error)),
            Err(_) => Err(BrowserVmError::ShutdownTimeout(SHUTDOWN_TIMEOUT)),
        }
    }

    async fn wait_until_ready(
        &mut self,
        timeout: Duration,
        local: SocketAddr,
    ) -> Result<Url, BrowserVmError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(500))
            .build()
            .map_err(BrowserVmError::HttpClient)?;
        let version = self.cdp_endpoint.join("/json/version")?;
        let started_at = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().map_err(BrowserVmError::InspectVmm)? {
                return Err(BrowserVmError::EarlyExit {
                    status,
                    log: read_log(&self.log),
                });
            }
            if let Ok(response) = client.get(version.clone()).send().await
                && response.status().is_success()
            {
                let metadata = serde_json::from_str::<CdpVersion>(
                    &response.text().await.map_err(BrowserVmError::HttpClient)?,
                )?;
                return local_websocket_endpoint(
                    Url::parse(&metadata.web_socket_debugger_url)?,
                    local,
                );
            }
            if started_at.elapsed() >= timeout {
                return Err(BrowserVmError::StartupTimeout {
                    endpoint: Box::new(self.cdp_endpoint.clone()),
                    timeout,
                    log: read_log(&self.log),
                });
            }
            time::sleep(Duration::from_millis(25)).await;
        }
    }
}

#[derive(Deserialize)]
struct CdpVersion {
    #[serde(rename = "webSocketDebuggerUrl")]
    web_socket_debugger_url: String,
}

impl Drop for BrowserVm {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

fn reserve_loopback_address() -> Result<SocketAddr, BrowserVmError> {
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        .map_err(BrowserVmError::ReservePort)?;
    listener.local_addr().map_err(BrowserVmError::ReservePort)
}

fn validate_file(
    path: &Path,
    error: impl FnOnce(PathBuf) -> BrowserVmError,
) -> Result<(), BrowserVmError> {
    path.is_file()
        .then_some(())
        .ok_or_else(|| error(path.to_path_buf()))
}

fn dynamic_library_path(directory: &Path) -> OsString {
    let mut value = directory.as_os_str().to_owned();
    if let Some(existing) = std::env::var_os("DYLD_LIBRARY_PATH")
        && !existing.is_empty()
    {
        value.push(":");
        value.push(existing);
    }
    value
}

fn local_websocket_endpoint(mut endpoint: Url, local: SocketAddr) -> Result<Url, BrowserVmError> {
    endpoint
        .set_host(Some(&local.ip().to_string()))
        .map_err(|_| BrowserVmError::InvalidWebSocketEndpoint(Box::new(endpoint.clone())))?;
    endpoint
        .set_port(Some(local.port()))
        .map_err(|()| BrowserVmError::InvalidWebSocketEndpoint(Box::new(endpoint.clone())))?;
    Ok(endpoint)
}

fn read_log(path: &Path) -> String {
    read_log_inner(path)
        .unwrap_or_else(|error| format!("failed to read {}: {error}", path.display()))
}

fn read_log_inner(path: &Path) -> Result<String, io::Error> {
    let mut file = fs::File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(MAX_ERROR_LOG_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity(usize::try_from(length - start).unwrap_or(0));
    file.read_to_end(&mut bytes)?;
    let log = String::from_utf8_lossy(&bytes);
    let log = log.trim();
    if start == 0 {
        Ok(log.to_owned())
    } else {
        Ok(format!("[earlier browser VM log bytes omitted]\n{log}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_do_not_launch_headless_or_enable_automation() {
        assert!(!BROWSER_SCRIPT.contains("--headless"));
        assert!(!BROWSER_SCRIPT.contains("--enable-automation"));
        assert!(BROWSER_SCRIPT.contains("Xvfb :99"));
        assert!(BROWSER_SCRIPT.contains("socat TCP-LISTEN:9223"));
        assert!(BROWSER_SCRIPT.contains("--remote-debugging-port=9222"));
    }

    #[test]
    fn startup_errors_retain_only_a_bounded_log_tail() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("browser.log");
        let prefix = "x".repeat(usize::try_from(MAX_ERROR_LOG_BYTES).unwrap());
        fs::write(&path, format!("{prefix}discarded\nlast useful line\n")).unwrap();

        let log = read_log(&path);

        assert!(log.starts_with("[earlier browser VM log bytes omitted]\n"));
        assert!(log.ends_with("last useful line"));
        assert!(log.len() <= usize::try_from(MAX_ERROR_LOG_BYTES).unwrap() + 64);
    }
}

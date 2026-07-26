use std::{
    collections::BTreeSet,
    fs::File,
    future::Future,
    io::{self, BufReader, Write},
    path::{Path, PathBuf},
    pin::Pin,
    process::Output,
    sync::Arc,
};

use async_trait::async_trait;
use nanocodex::{
    AgentEvent, AgentEvents, Nanocodex, NanocodexError, OpenAiAuth, Prompt, ResponseItem,
    SessionSnapshot, Thinking, Tools, UpdatePlanTool,
};
use nanocodex_vm::{VmToolSession, VmTools};
use nanovm::{GuestCommand, KrunVm, Network, SharedDirectory, VmConfig};
use serde::{Deserialize, Serialize};
use tempfile::{NamedTempFile, TempDir, TempPath};
use thiserror::Error;
use tokio::{process::Command, sync::mpsc};

use crate::{AgentCapabilities, CapabilityName, EgressContext, EgressLease, EgressProvider};

// libkrun currently wraps every guest argument in double quotes without
// escaping embedded quotes. Keep this script printable ASCII and quote-free;
// all interpolated values are server-generated guest paths and mount tags.
const AGENT_VMM_SCRIPT: &str = concat!(
    "set -eu; ",
    "workspace=$1; ",
    "runtime=$2; ",
    "shift 2; ",
    "mkdir -p $workspace; ",
    "mount -t virtiofs nanocentaur-workspace $workspace; ",
    "while [ $# -gt 0 ]; do ",
    "tag=$1; target=$2; shift 2; ",
    "mkdir -p $target; ",
    "mount -t virtiofs -o ro $tag $target; ",
    "done; ",
    "exec $runtime $workspace"
);
/// Immutable inputs used to construct or resume one hosted agent harness.
#[derive(Clone)]
pub struct AgentSpec {
    pub agent_id: String,
    pub principal: String,
    pub instructions: Option<String>,
    pub thinking: Option<Thinking>,
    pub capabilities: AgentCapabilities,
    pub snapshot: Option<SessionSnapshot>,
}

/// A native Nanocodex event stays typed through the actor and SSE boundary.
#[derive(Clone, Debug)]
pub struct RuntimeEvent(pub AgentEvent);

pub struct AgentRunResult {
    pub final_message: String,
    pub snapshot: Option<SessionSnapshot>,
}

pub struct ManagedTurn {
    pub control: Arc<dyn ManagedTurnControl>,
    pub result: Pin<Box<dyn Future<Output = Result<AgentRunResult, AgentError>> + Send>>,
}

pub struct SpawnedAgent {
    pub agent: Arc<dyn ManagedAgent>,
    pub events: mpsc::UnboundedReceiver<RuntimeEvent>,
}

#[async_trait]
pub trait ManagedAgent: Send + Sync {
    async fn prompt(&self, prompt: Prompt) -> Result<ManagedTurn, AgentError>;
}

#[async_trait]
pub trait ManagedTurnControl: Send + Sync {
    async fn steer(&self, prompt: Prompt) -> Result<(), AgentError>;
    async fn cancel(&self) -> Result<(), AgentError>;
}

#[async_trait]
pub trait ManagedAgentFactory: Send + Sync {
    async fn create(&self, spec: AgentSpec) -> Result<SpawnedAgent, AgentError>;
}

/// Nanocodex runs on the host while all workspace tools execute in a dedicated
/// libkrun VM. VMs and root filesystems are disposable; the host workspace is
/// an explicit mounted resource and the durable session lives in `SQLite`.
pub struct NanocodexAgentFactory {
    auth: OpenAiAuth,
    vmm_executable: PathBuf,
    rootfs_template: PathBuf,
    state_directory: PathBuf,
    guest_runtime: String,
    guest_shell: String,
    egress: Arc<dyn EgressProvider>,
}

impl NanocodexAgentFactory {
    #[must_use]
    pub fn new(
        auth: OpenAiAuth,
        vmm_executable: impl Into<PathBuf>,
        rootfs_template: impl Into<PathBuf>,
        state_directory: impl Into<PathBuf>,
        egress: Arc<dyn EgressProvider>,
    ) -> Self {
        Self {
            auth,
            vmm_executable: vmm_executable.into(),
            rootfs_template: rootfs_template.into(),
            state_directory: state_directory.into(),
            guest_runtime: "/usr/local/bin/nanocodex-vm-guest".to_owned(),
            guest_shell: "sh".to_owned(),
            egress,
        }
    }

    #[must_use]
    pub fn guest_runtime(mut self, guest_runtime: impl Into<String>) -> Self {
        self.guest_runtime = guest_runtime.into();
        self
    }
}

#[async_trait]
impl ManagedAgentFactory for NanocodexAgentFactory {
    async fn create(&self, spec: AgentSpec) -> Result<SpawnedAgent, AgentError> {
        let egress_capabilities: BTreeSet<CapabilityName> = spec
            .capabilities
            .names()
            .iter()
            .filter(|capability| {
                !capability.as_str().starts_with("tools.")
                    && !capability.as_str().starts_with("agent.")
            })
            .cloned()
            .collect();
        let egress = self
            .egress
            .acquire(
                &EgressContext {
                    agent_id: spec.agent_id.clone(),
                    principal: spec.principal.clone(),
                },
                &egress_capabilities,
            )
            .await?;
        let (runtime_directory, rootfs, workspace) = self.agent_paths(&spec.agent_id)?;
        let workspace = workspace
            .into_os_string()
            .into_string()
            .map_err(|_| AgentError::WorkspaceNotUtf8)?;
        let vmm_config = VmmProcessConfig::from_lease(
            rootfs,
            self.guest_runtime.clone(),
            workspace.clone(),
            &egress,
        );
        let config_path = write_private_vmm_config(&vmm_config)?;
        let mut command = Command::new(&self.vmm_executable);
        configure_vmm_host_environment(&mut command);
        command
            .arg("vmm")
            .arg("--config")
            .arg(config_path.as_os_str());
        let vm = VmToolSession::spawn(&mut command)?;
        let vm_tools = VmTools::new(vm.clone());
        let tools = Tools::builder()
            .without_defaults()
            .web_search(spec.capabilities.contains("tools.web_search"))
            .image_generation(spec.capabilities.contains("tools.image_generation"))
            .working_directory(workspace.clone())
            .default_shell(self.guest_shell.clone())
            .tool(vm_tools.exec_command_tool())
            .tool(vm_tools.write_stdin_tool())
            .tool(vm_tools.apply_patch_tool())
            .tool(vm_tools.view_image_tool())
            .tool(UpdatePlanTool::new())
            .build()?;

        let mut builder = Nanocodex::builder(self.auth.clone())
            .session_id(spec.agent_id)
            .workspace(&workspace)
            .tools(tools);
        if let Some(instructions) = spec.instructions {
            builder = builder.instructions(instructions);
        }
        if let Some(thinking) = spec.thinking {
            builder = builder.thinking(thinking);
        }
        if let Some(snapshot) = spec.snapshot {
            builder = builder.resume(rebase_snapshot(&snapshot, &workspace)?);
        }
        let (agent, events) = builder.build()?;
        let (event_sender, event_receiver) = mpsc::unbounded_channel();
        tokio::spawn(forward_events(events, event_sender));

        Ok(SpawnedAgent {
            agent: Arc::new(NanocodexManagedAgent {
                agent,
                _vm: vm,
                _egress: egress,
                _vmm_config: config_path,
                _runtime_directory: runtime_directory,
            }),
            events: event_receiver,
        })
    }
}

fn configure_vmm_host_environment(command: &mut Command) {
    // The VMM only needs its private typed config. In particular it must not
    // inherit host secret-provider variables or model keys.
    command.env_clear().env("PATH", "/usr/bin:/bin");

    // libkrunfw is dynamically loaded on macOS. Preserve only that operational
    // loader path when the signed runner configured one; no general host
    // environment is inherited by the child.
    if let Some(value) = std::env::var_os("DYLD_LIBRARY_PATH") {
        command.env("DYLD_LIBRARY_PATH", value);
    }
}

impl NanocodexAgentFactory {
    fn agent_paths(&self, agent_id: &str) -> Result<(TempDir, AgentRootfs, PathBuf), AgentError> {
        let runtimes = self.state_directory.join("runtimes");
        std::fs::create_dir_all(&runtimes)?;
        let directory = tempfile::Builder::new()
            .prefix(&format!("{agent_id}-"))
            .tempdir_in(runtimes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
        }
        let rootfs = if self.rootfs_template.is_file() {
            let destination = directory.path().join("rootfs.ext4");
            reflink_copy::reflink_or_copy(&self.rootfs_template, &destination)?;
            AgentRootfs::Ext4(destination)
        } else if self.rootfs_template.is_dir() {
            let destination = directory.path().join("rootfs");
            copy_directory(&self.rootfs_template, &destination)?;
            AgentRootfs::Directory(destination)
        } else {
            return Err(AgentError::InvalidRootfsTemplate(
                self.rootfs_template.clone(),
            ));
        };
        let workspace = self.state_directory.join("workspaces").join(agent_id);
        std::fs::create_dir_all(&workspace)?;
        Ok((directory, rootfs, std::fs::canonicalize(workspace)?))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "path")]
enum AgentRootfs {
    Directory(PathBuf),
    Ext4(PathBuf),
}

struct NanocodexManagedAgent {
    agent: Nanocodex,
    _vm: VmToolSession,
    _egress: EgressLease,
    _vmm_config: TempPath,
    _runtime_directory: TempDir,
}

#[async_trait]
impl ManagedAgent for NanocodexManagedAgent {
    async fn prompt(&self, prompt: Prompt) -> Result<ManagedTurn, AgentError> {
        let turn = self.agent.prompt(prompt).await?;
        let control = Arc::new(NanocodexTurnControl(turn.control()));
        Ok(ManagedTurn {
            control,
            result: Box::pin(async move {
                let result = turn.result().await.map_err(map_nanocodex_error)?;
                Ok(AgentRunResult {
                    final_message: result.final_message.clone(),
                    snapshot: Some(result.snapshot()),
                })
            }),
        })
    }
}

struct NanocodexTurnControl(nanocodex::TurnControl);

#[async_trait]
impl ManagedTurnControl for NanocodexTurnControl {
    async fn steer(&self, prompt: Prompt) -> Result<(), AgentError> {
        self.0.steer(prompt).await.map_err(map_nanocodex_error)
    }

    async fn cancel(&self) -> Result<(), AgentError> {
        self.0.cancel().await.map_err(map_nanocodex_error)
    }
}

fn map_nanocodex_error(error: NanocodexError) -> AgentError {
    match error {
        NanocodexError::TurnNotSteerable => AgentError::TurnNotSteerable,
        NanocodexError::SteerQueueFull => AgentError::SteerQueueFull,
        NanocodexError::TurnNotCancellable => AgentError::TurnNotCancellable,
        error => AgentError::Nanocodex(error),
    }
}

async fn forward_events(mut events: AgentEvents, sender: mpsc::UnboundedSender<RuntimeEvent>) {
    while let Some(event) = events.recv().await {
        if sender.send(RuntimeEvent(event)).is_err() {
            return;
        }
    }
}

#[derive(Deserialize, Serialize)]
struct VmmProcessConfig {
    rootfs: AgentRootfs,
    guest_runtime: String,
    workspace: String,
    network: VmmNetwork,
    guest_environment: Vec<(String, String)>,
    guest_mounts: Vec<VmmMount>,
}

#[derive(Deserialize, Serialize)]
struct VmmMount {
    tag: String,
    host_path: PathBuf,
    guest_path: PathBuf,
}

#[derive(Deserialize, Serialize)]
struct VmmCommandProcessConfig {
    rootfs: AgentRootfs,
    command: Vec<String>,
    network: VmmNetwork,
    guest_environment: Vec<(String, String)>,
    guest_mounts: Vec<VmmMount>,
}

impl VmmProcessConfig {
    fn from_lease(
        rootfs: AgentRootfs,
        guest_runtime: String,
        workspace: String,
        lease: &EgressLease,
    ) -> Self {
        let network = match lease.network() {
            Network::Disabled => VmmNetwork::Disabled,
            Network::Internet => VmmNetwork::Internet,
            Network::Gvproxy {
                socket,
                mac_address,
            } => VmmNetwork::Gvproxy {
                socket: socket.clone(),
                mac_address: *mac_address,
            },
        };
        Self {
            rootfs,
            guest_runtime,
            workspace,
            network,
            guest_environment: lease.guest_environment().to_vec(),
            guest_mounts: lease
                .guest_mounts()
                .iter()
                .map(|mount| VmmMount {
                    tag: mount.tag.clone(),
                    host_path: mount.host_path.clone(),
                    guest_path: mount.guest_path.clone(),
                })
                .collect(),
        }
    }
}

impl VmmCommandProcessConfig {
    fn from_lease(rootfs: AgentRootfs, command: Vec<String>, lease: &EgressLease) -> Self {
        let network = match lease.network() {
            Network::Disabled => VmmNetwork::Disabled,
            Network::Internet => VmmNetwork::Internet,
            Network::Gvproxy {
                socket,
                mac_address,
            } => VmmNetwork::Gvproxy {
                socket: socket.clone(),
                mac_address: *mac_address,
            },
        };
        Self {
            rootfs,
            command,
            network,
            guest_environment: lease.guest_environment().to_vec(),
            guest_mounts: lease
                .guest_mounts()
                .iter()
                .map(|mount| VmmMount {
                    tag: mount.tag.clone(),
                    host_path: mount.host_path.clone(),
                    guest_path: mount.guest_path.clone(),
                })
                .collect(),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
enum VmmNetwork {
    Disabled,
    Internet,
    Gvproxy {
        socket: PathBuf,
        mac_address: [u8; 6],
    },
}

impl From<VmmNetwork> for Network {
    fn from(value: VmmNetwork) -> Self {
        match value {
            VmmNetwork::Disabled => Self::Disabled,
            VmmNetwork::Internet => Self::Internet,
            VmmNetwork::Gvproxy {
                socket,
                mac_address,
            } => Self::Gvproxy {
                socket,
                mac_address,
            },
        }
    }
}

fn write_private_vmm_config(config: &VmmProcessConfig) -> Result<TempPath, AgentError> {
    let mut file = NamedTempFile::new()?;
    serde_json::to_writer(&mut file, config)?;
    file.flush()?;
    Ok(file.into_temp_path())
}

fn write_private_vmm_command_config(
    config: &VmmCommandProcessConfig,
) -> Result<TempPath, AgentError> {
    let mut file = NamedTempFile::new()?;
    serde_json::to_writer(&mut file, config)?;
    file.flush()?;
    Ok(file.into_temp_path())
}

/// Nanocodex snapshots intentionally own their typed model history, but their
/// serialized workspace is a runtime location. Rebase only that location when
/// waking the harness against a newly provisioned sandbox.
fn rebase_snapshot(
    snapshot: &SessionSnapshot,
    workspace: &str,
) -> Result<SessionSnapshot, AgentError> {
    let mut portable: PortableSessionSnapshot =
        serde_json::from_slice(&serde_json::to_vec(&snapshot)?)?;
    workspace.clone_into(&mut portable.workspace);
    Ok(serde_json::from_slice(&serde_json::to_vec(&portable)?)?)
}

#[derive(Deserialize, Serialize)]
struct PortableSessionSnapshot {
    version: u32,
    model: String,
    lineage_id: String,
    prompt_cache_key: String,
    workspace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_prefix: Option<Vec<ResponseItem>>,
    canonical_context: ResponseItem,
    history: Vec<ResponseItem>,
}

/// Enters the blocking libkrun VMM loop from the server's hidden child mode.
///
/// # Errors
///
/// Returns an error when the VM configuration cannot be read or the VMM
/// cannot be constructed or run.
pub fn run_vmm(config_path: &Path) -> Result<(), AgentError> {
    let config: VmmProcessConfig =
        serde_json::from_reader(BufReader::new(File::open(config_path)?))?;
    let mut arguments = vec![
        "-c".to_owned(),
        AGENT_VMM_SCRIPT.to_owned(),
        "nanocentaur-vmm".to_owned(),
        config.workspace.clone(),
        config.guest_runtime,
    ];
    for mount in &config.guest_mounts {
        arguments.push(mount.tag.clone());
        arguments.push(mount.guest_path.to_string_lossy().into_owned());
    }
    let command = config.guest_environment.into_iter().fold(
        GuestCommand::new("/bin/sh").args(arguments),
        |command, (name, value)| command.env(name, value),
    );
    let mut vm = match config.rootfs {
        AgentRootfs::Directory(path) => VmConfig::new(path),
        AgentRootfs::Ext4(path) => VmConfig::ext4(path),
    }
    .cpus(2)
    .memory_mib(1_024)
    .network(config.network.into())
    .shared_directory(SharedDirectory::read_write(
        "nanocentaur-workspace",
        &config.workspace,
    ));
    for mount in config.guest_mounts {
        vm = vm.shared_directory(SharedDirectory::read_only(mount.tag, mount.host_path));
    }
    KrunVm::new(&vm)?.run(&command)?;
    Ok(())
}

/// Runs one command in an isolated copy of a rootfs using an existing egress
/// lease. The VMM child receives proxy credentials and CA mounts, but never
/// inherits host secret-manager or model credentials.
///
/// # Errors
///
/// Returns an error when the rootfs cannot be copied, the private VMM config
/// cannot be created, or the VMM child cannot be spawned.
pub async fn run_guest_command(
    vmm_executable: impl AsRef<Path>,
    rootfs_template: impl AsRef<Path>,
    lease: &EgressLease,
    command: Vec<String>,
) -> Result<Output, AgentError> {
    if command.is_empty() {
        return Err(AgentError::EmptyGuestCommand);
    }
    let runtime_directory = tempfile::Builder::new()
        .prefix("nanocentaur-command-")
        .tempdir()?;
    let rootfs_template = rootfs_template.as_ref();
    let rootfs = if rootfs_template.is_file() {
        let destination = runtime_directory.path().join("rootfs.ext4");
        reflink_copy::reflink_or_copy(rootfs_template, &destination)?;
        if !lease.guest_mounts().is_empty() {
            return Err(AgentError::OneShotExt4MountsUnsupported);
        }
        AgentRootfs::Ext4(destination)
    } else if rootfs_template.is_dir() {
        let destination = runtime_directory.path().join("rootfs");
        copy_directory(rootfs_template, &destination)?;
        copy_egress_mounts_into_rootfs(&destination, lease.guest_mounts())?;
        AgentRootfs::Directory(destination)
    } else {
        return Err(AgentError::InvalidRootfsTemplate(
            rootfs_template.to_owned(),
        ));
    };
    let mut config = VmmCommandProcessConfig::from_lease(rootfs, command, lease);
    config.guest_mounts.clear();
    let config_path = write_private_vmm_command_config(&config)?;
    let mut process = Command::new(vmm_executable.as_ref());
    configure_vmm_host_environment(&mut process);
    process
        .arg("one-shot-vmm")
        .arg("--config")
        .arg(config_path.as_os_str());
    let output = process.output().await?;
    drop(config_path);
    drop(runtime_directory);
    Ok(output)
}

/// Enters the blocking libkrun VMM loop for a one-shot guest command.
///
/// # Errors
///
/// Returns an error when the private configuration is invalid or libkrun
/// cannot configure and boot the VM.
pub fn run_vmm_command(config_path: &Path) -> Result<(), AgentError> {
    let config: VmmCommandProcessConfig =
        serde_json::from_reader(BufReader::new(File::open(config_path)?))?;
    let (program, arguments) = config
        .command
        .split_first()
        .ok_or(AgentError::EmptyGuestCommand)?;
    let command = config.guest_environment.into_iter().fold(
        GuestCommand::new(program).args(arguments),
        |command, (name, value)| command.env(name, value),
    );
    let mut vm = match config.rootfs {
        AgentRootfs::Directory(path) => VmConfig::new(path),
        AgentRootfs::Ext4(path) => VmConfig::ext4(path),
    }
    .cpus(2)
    .memory_mib(1_024)
    .network(config.network.into());
    for mount in config.guest_mounts {
        vm = vm.shared_directory(SharedDirectory::read_only(mount.tag, mount.host_path));
    }
    KrunVm::new(&vm)?.run(&command)?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("the targeted turn is not active for steering")]
    TurnNotSteerable,
    #[error("the active turn's steering queue is full")]
    SteerQueueFull,
    #[error("the targeted turn is no longer cancellable")]
    TurnNotCancellable,
    #[error("agent egress setup failed")]
    Egress(#[from] crate::EgressError),
    #[error("VM configuration I/O failed")]
    Io(#[from] io::Error),
    #[error("VM configuration serialization failed")]
    Json(#[from] serde_json::Error),
    #[error("agent workspace path is not valid UTF-8")]
    WorkspaceNotUtf8,
    #[error("rootfs template is not a directory or ext4 file: {0}")]
    InvalidRootfsTemplate(PathBuf),
    #[error("guest command must contain a program")]
    EmptyGuestCommand,
    #[error("one-shot ext4 commands do not support egress file mounts")]
    OneShotExt4MountsUnsupported,
    #[error("VM tool session failed")]
    VmSession(#[from] nanocodex_vm::VmToolSessionError),
    #[error("VM setup failed")]
    Vm(#[from] nanovm::VmError),
    #[error("Nanocodex setup or execution failed")]
    Nanocodex(#[from] nanocodex::NanocodexError),
    #[error("Nanocodex tool setup failed")]
    Tools(#[from] nanocodex::ToolsBuildError),
    #[error("managed agent failed: {0}")]
    Backend(String),
}

fn copy_directory(source: &Path, destination: &Path) -> Result<(), io::Error> {
    std::fs::create_dir(destination)?;
    std::fs::set_permissions(
        destination,
        std::fs::symlink_metadata(source)?.permissions(),
    )?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let source = entry.path();
        let destination = destination.join(entry.file_name());
        let metadata = std::fs::symlink_metadata(&source)?;
        if metadata.file_type().is_symlink() {
            copy_symlink(&source, &destination)?;
        } else if metadata.is_dir() {
            copy_directory(&source, &destination)?;
        } else if metadata.is_file() {
            reflink_copy::reflink_or_copy(&source, &destination)?;
            std::fs::set_permissions(&destination, metadata.permissions())?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported rootfs entry: {}", source.display()),
            ));
        }
    }
    Ok(())
}

fn copy_egress_mounts_into_rootfs(
    rootfs: &Path,
    mounts: &[crate::EgressMount],
) -> Result<(), io::Error> {
    for mount in mounts {
        let relative = mount
            .guest_path
            .strip_prefix("/")
            .map_err(|_| io::Error::other("egress guest mount must be absolute"))?;
        if relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(io::Error::other("egress guest mount path is unsafe"));
        }
        let destination = rootfs.join(relative);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if mount.host_path.is_dir() {
            copy_directory(&mount.host_path, &destination)?;
        } else if mount.host_path.is_file() {
            reflink_copy::reflink_or_copy(&mount.host_path, &destination)?;
        } else {
            return Err(io::Error::other(
                "egress host mount is not a file or directory",
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(source: &Path, destination: &Path) -> Result<(), io::Error> {
    std::os::unix::fs::symlink(std::fs::read_link(source)?, destination)
}

#[cfg(not(unix))]
fn copy_symlink(_source: &Path, _destination: &Path) -> Result<(), io::Error> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "rootfs symlinks require a Unix host",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vmm_child_has_only_the_operational_host_environment_allowlist() {
        let mut command = Command::new("nanocentaur-server");
        configure_vmm_host_environment(&mut command);

        let names = command
            .as_std()
            .get_envs()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect::<BTreeSet<_>>();
        assert!(names.contains("PATH"));
        assert!(
            names
                .iter()
                .all(|name| matches!(name.as_str(), "PATH" | "DYLD_LIBRARY_PATH"))
        );
        assert!(!names.iter().any(|name| {
            name.starts_with("NANOCENTAUR_SECRET_")
                || name.contains("API_KEY")
                || name.contains("TOKEN")
        }));
    }

    #[test]
    fn vmm_shell_scripts_fit_libkrun_argument_encoding() {
        assert!(
            AGENT_VMM_SCRIPT
                .bytes()
                .all(|byte| byte.is_ascii_graphic() || byte == b' ')
        );
        assert!(!AGENT_VMM_SCRIPT.contains('"'));
    }
}

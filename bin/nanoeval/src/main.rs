mod cleanup;
mod compare;
mod config;
mod disk;
mod eval;
mod inspect;
mod observability;
mod vm_image;
mod vm_network;

use std::{
    collections::BTreeMap,
    io::{self, Write},
    path::{Path, PathBuf},
    str::FromStr,
    time::Instant,
};

use clap::{CommandFactory, Parser, Subcommand};
use eyre::{Result, eyre};
use nanoeval::{Task, VerifierCollect, VerifierEnvironmentMode};
use nanovm::{BlockDevice, GuestCommand, KrunVm, Network, SharedDirectory, VmConfig};
use serde::Serialize;

#[derive(Parser)]
#[command(
    name = "nanoeval",
    version,
    about = "Fast, Docker-free evaluation for coding agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run fresh Nanocodex attempts and retain Harbor-compatible outputs.
    Run(Box<eval::Eval>),

    /// Load, validate, and inspect a benchmark task directory.
    Task {
        /// Directory containing task.toml, instruction.md, and tests/test.sh.
        directory: PathBuf,

        /// Emit the complete loaded task as JSON.
        #[arg(long)]
        json: bool,

        /// Include the complete prompt in human-readable output.
        #[arg(long, conflicts_with = "json")]
        prompt: bool,
    },

    /// Explain a retained Harbor job or trial and surface exact failure evidence.
    Inspect(inspect::Inspect),

    /// Compare a task or retained job with Harbor's public archive.
    Compare(compare::Compare),

    /// Remove disposable VM disks from completed retained trials.
    Cleanup(cleanup::Cleanup),

    /// Run one command in a libkrun microVM.
    Vm {
        #[command(subcommand)]
        command: VmCommand,
    },
}

#[derive(Subcommand)]
enum VmCommand {
    /// Prepare one or more tasks' Linux/ARM64 root disks without running agents.
    Prepare {
        /// Terminal-Bench task directory. Repeat to prepare several environments.
        #[arg(
            long = "task",
            value_name = "DIRECTORY",
            required_unless_present = "suites"
        )]
        tasks: Vec<PathBuf>,

        /// Terminal-Bench suite directory whose immediate task children should prepare.
        #[arg(
            long = "suite",
            value_name = "DIRECTORY",
            required_unless_present = "tasks"
        )]
        suites: Vec<PathBuf>,

        /// Content-addressed VM cache directory.
        #[arg(long, default_value = ".cache/vm")]
        cache: PathBuf,

        /// Resolve the image reference at the registry even when locally cached.
        #[arg(long)]
        refresh: bool,
    },

    /// Boot a root filesystem and replace Nanoeval with the guest command.
    Run {
        /// Linux root filesystem directory exposed to the guest.
        #[arg(long)]
        root: PathBuf,

        /// Treat `--root` as a raw ext4 block image instead of a virtiofs directory.
        #[arg(long)]
        ext4: bool,

        /// Number of virtual CPUs.
        #[arg(long, default_value_t = 2)]
        cpus: u8,

        /// Guest memory in MiB.
        #[arg(long, default_value_t = 1024)]
        memory_mib: u32,

        /// Gvproxy unixgram socket used for an isolated virtio-net interface.
        #[arg(long, value_name = "SOCKET", conflicts_with = "no_network")]
        network_socket: Option<PathBuf>,

        /// Do not attach a guest network device.
        #[arg(long)]
        no_network: bool,

        /// Read-only directory containing the guest tool runtime.
        #[arg(long, value_name = "DIRECTORY")]
        runtime_directory: Option<PathBuf>,

        /// Writable host directory exposed as TAG through virtiofs.
        #[arg(long, value_name = "TAG=DIRECTORY")]
        writable_share: Vec<WritableShare>,

        /// Immutable block image attached as ID after the root disk.
        #[arg(long, value_name = "ID=IMAGE")]
        read_only_disk: Vec<ReadOnlyDisk>,

        /// Private writable block image attached as ID after the root disk.
        #[arg(long, value_name = "ID=IMAGE")]
        writable_disk: Vec<ReadOnlyDisk>,

        /// Environment entry inherited by the guest's initial process.
        #[arg(long = "env", value_name = "NAME=VALUE")]
        environment: Vec<GuestEnvironment>,

        /// Executable and arguments to run inside the guest.
        #[arg(required = true, trailing_var_arg = true)]
        guest_command: Vec<std::ffi::OsString>,
    },
}

#[derive(Clone)]
struct WritableShare {
    tag: String,
    directory: PathBuf,
}

#[derive(Clone)]
struct ReadOnlyDisk {
    id: String,
    image: PathBuf,
}

#[derive(Clone)]
struct GuestEnvironment {
    name: String,
    value: String,
}

#[derive(Debug, thiserror::Error)]
enum WritableShareParseError {
    #[error("writable share must have the form TAG=DIRECTORY")]
    MissingSeparator,

    #[error("writable share tag must contain only ASCII letters, digits, '-' or '_'")]
    InvalidTag,

    #[error("writable share directory must not be empty")]
    EmptyDirectory,
}

impl FromStr for WritableShare {
    type Err = WritableShareParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (tag, directory) = value
            .split_once('=')
            .ok_or(WritableShareParseError::MissingSeparator)?;
        if tag.is_empty()
            || !tag
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(WritableShareParseError::InvalidTag);
        }
        if directory.is_empty() {
            return Err(WritableShareParseError::EmptyDirectory);
        }
        Ok(Self {
            tag: tag.to_owned(),
            directory: PathBuf::from(directory),
        })
    }
}

impl FromStr for ReadOnlyDisk {
    type Err = WritableShareParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let share = WritableShare::from_str(value)?;
        Ok(Self {
            id: share.tag,
            image: share.directory,
        })
    }
}

impl FromStr for GuestEnvironment {
    type Err = GuestEnvironmentParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (name, value) = value
            .split_once('=')
            .ok_or(GuestEnvironmentParseError::MissingSeparator)?;
        let mut bytes = name.bytes();
        if !bytes
            .next()
            .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
            || !bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
        {
            return Err(GuestEnvironmentParseError::InvalidName);
        }
        Ok(Self {
            name: name.to_owned(),
            value: value.to_owned(),
        })
    }
}

#[derive(Debug, thiserror::Error)]
enum GuestEnvironmentParseError {
    #[error("guest environment must have the form NAME=VALUE")]
    MissingSeparator,

    #[error("guest environment name must be a shell identifier")]
    InvalidName,
}

fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    enable_paint();
    let cli = Cli::parse();

    if let Some(Command::Vm {
        command:
            VmCommand::Run {
                root,
                ext4,
                cpus,
                memory_mib,
                network_socket,
                no_network,
                runtime_directory,
                writable_share,
                read_only_disk,
                writable_disk,
                environment,
                guest_command,
            },
    }) = &cli.command
    {
        return run_vm(RunVm {
            root,
            ext4: *ext4,
            cpus: *cpus,
            memory_mib: *memory_mib,
            network_socket: network_socket.as_deref(),
            no_network: *no_network,
            runtime_directory: runtime_directory.as_deref(),
            writable_shares: writable_share,
            read_only_disks: read_only_disk,
            writable_disks: writable_disk,
            environment,
            guest_command,
        });
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(cli))
}

fn enable_paint() {
    let enable = yansi::Condition::os_support() && yansi::Condition::tty_and_color_live();
    yansi::whenever(yansi::Condition::cached(enable));
}

#[derive(Clone, Copy)]
struct RunVm<'a> {
    root: &'a Path,
    ext4: bool,
    cpus: u8,
    memory_mib: u32,
    network_socket: Option<&'a Path>,
    no_network: bool,
    runtime_directory: Option<&'a Path>,
    writable_shares: &'a [WritableShare],
    read_only_disks: &'a [ReadOnlyDisk],
    writable_disks: &'a [ReadOnlyDisk],
    environment: &'a [GuestEnvironment],
    guest_command: &'a [std::ffi::OsString],
}

fn run_vm(input: RunVm<'_>) -> Result<()> {
    let (program, arguments) = input
        .guest_command
        .split_first()
        .ok_or_else(|| eyre!("guest command must not be empty"))?;
    let mut config = if input.ext4 {
        VmConfig::ext4(input.root)
    } else {
        VmConfig::new(input.root)
    }
    .cpus(input.cpus)
    .memory_mib(input.memory_mib);
    if let Some(socket) = input.network_socket {
        config = config.network(Network::gvproxy(socket));
    } else if input.no_network {
        config = config.network(Network::Disabled);
    }
    if let Some(directory) = input.runtime_directory {
        config = config.shared_directory(SharedDirectory::read_only("nanoeval-tools", directory));
    }
    for share in input.writable_shares {
        config = config.shared_directory(SharedDirectory::read_write(
            share.tag.clone(),
            share.directory.clone(),
        ));
    }
    for disk in input.read_only_disks {
        config = config.block_device(BlockDevice::read_only(disk.id.clone(), disk.image.clone()));
    }
    for disk in input.writable_disks {
        config = config.block_device(BlockDevice::read_write(disk.id.clone(), disk.image.clone()));
    }
    let mut command = GuestCommand::new(program).args(arguments);
    for entry in input.environment {
        command = command.env(&entry.name, &entry.value);
    }
    KrunVm::new(&config)?.run(&command)?;
    Ok(())
}

async fn prepare_tasks(
    tasks: Vec<PathBuf>,
    suites: Vec<PathBuf>,
    cache: PathBuf,
    refresh: bool,
) -> Result<()> {
    let preparation_started = Instant::now();
    let tasks = eval::load_task_paths(tasks, suites)?
        .into_iter()
        .map(Task::load)
        .collect::<Result<Vec<_>, _>>()?;
    let policy = if refresh {
        vm_image::CachePolicy::Refresh
    } else {
        vm_image::CachePolicy::Reuse
    };
    // Resolve the running, entitled VMM executable before a nested guest
    // runtime build can cause Cargo's runner cache to rotate paths.
    let vmm = std::env::current_exe()?;
    let runtime_started = Instant::now();
    let runtime_image = eval::prepare_vm_guest_runtime().await?;
    let runtime_duration = runtime_started.elapsed();
    let builder = vm_image::VmImageBuilder::new(vmm, runtime_image, ".cache/libkrunfw/libkrunfw");
    let mut cache_hits = 0_usize;
    let mut cache_creations = 0_usize;
    let mut failures = Vec::new();
    for task in tasks {
        let task_started = Instant::now();
        let prepared =
            match vm_image::PreparedRootDisk::prepare(&task, &cache, policy, &builder).await {
                Ok(prepared) => prepared,
                Err(error) => {
                    eprintln!(
                        "{}: failed duration={:.3?}\n{error:#}",
                        task.name(),
                        task_started.elapsed()
                    );
                    failures.push(task.name().to_owned());
                    continue;
                }
            };
        match prepared.disk_status() {
            vm_image::DiskStatus::Hit => cache_hits += 1,
            vm_image::DiskStatus::Created => cache_creations += 1,
        }
        eprintln!(
            "{}: manifest={} ({}) root_disk={} duration={:.3?}",
            task.name(),
            prepared.manifest_digest(),
            prepared.manifest_source().as_str(),
            prepared.disk_status().as_str(),
            task_started.elapsed()
        );
        println!("{}", prepared.path().display());
        if task.verifier().environment_mode() == VerifierEnvironmentMode::Separate {
            let verifier_started = Instant::now();
            let verifier =
                match vm_image::PreparedRootDisk::prepare_verifier(&task, &cache, policy, &builder)
                    .await
                {
                    Ok(verifier) => verifier,
                    Err(error) => {
                        eprintln!(
                            "{} verifier: failed duration={:.3?}\n{error:#}",
                            task.name(),
                            verifier_started.elapsed()
                        );
                        failures.push(format!("{} verifier", task.name()));
                        continue;
                    }
                };
            match verifier.disk_status() {
                vm_image::DiskStatus::Hit => cache_hits += 1,
                vm_image::DiskStatus::Created => cache_creations += 1,
            }
            eprintln!(
                "{} verifier: manifest={} ({}) root_disk={} duration={:.3?}",
                task.name(),
                verifier.manifest_digest(),
                verifier.manifest_source().as_str(),
                verifier.disk_status().as_str(),
                verifier_started.elapsed()
            );
            println!("{}", verifier.path().display());
        }
    }
    eprintln!(
        "VM preparation: runtime={runtime_duration:.3?} environments={} hits={cache_hits} created={cache_creations} failed={} total={:.3?}",
        cache_hits + cache_creations,
        failures.len(),
        preparation_started.elapsed()
    );
    if failures.is_empty() {
        Ok(())
    } else {
        Err(eyre!(
            "{} VM environment(s) failed preparation: {}",
            failures.len(),
            failures.join(", ")
        ))
    }
}

async fn run(cli: Cli) -> Result<()> {
    let Some(command) = cli.command else {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };

    match command {
        Command::Run(command) => command.run().await?,
        Command::Task {
            directory,
            json,
            prompt,
        } => {
            let task = Task::load(directory)?;
            let output = TaskOutput::from(&task);
            let stdout = io::stdout();
            let mut stdout = stdout.lock();
            if json {
                serde_json::to_writer_pretty(&mut stdout, &output)?;
                writeln!(stdout)?;
            } else {
                output.write_human(&mut stdout, prompt)?;
            }
        }
        Command::Inspect(command) => command.run()?,
        Command::Compare(command) => command.run().await?,
        Command::Cleanup(command) => command.run()?,
        Command::Vm {
            command:
                VmCommand::Prepare {
                    tasks,
                    suites,
                    cache,
                    refresh,
                },
        } => prepare_tasks(tasks, suites, cache, refresh).await?,
        Command::Vm {
            command:
                VmCommand::Run {
                    root,
                    ext4,
                    cpus,
                    memory_mib,
                    network_socket,
                    no_network,
                    runtime_directory,
                    writable_share,
                    read_only_disk,
                    writable_disk,
                    environment,
                    guest_command,
                },
        } => {
            run_vm(RunVm {
                root: &root,
                ext4,
                cpus,
                memory_mib,
                network_socket: network_socket.as_deref(),
                no_network,
                runtime_directory: runtime_directory.as_deref(),
                writable_shares: &writable_share,
                read_only_disks: &read_only_disk,
                writable_disks: &writable_disk,
                environment: &environment,
                guest_command: &guest_command,
            })?;
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct TaskOutput<'a> {
    name: &'a str,
    description: &'a str,
    root: &'a Path,
    prompt: &'a str,
    image: &'a str,
    agent_timeout_sec: f64,
    verifier: VerifierOutput<'a>,
    artifacts: &'a [PathBuf],
    resources: ResourcesOutput,
    network: &'static str,
    environment: &'a BTreeMap<String, String>,
    requires_compose: bool,
}

#[derive(Serialize)]
struct VerifierOutput<'a> {
    script: &'a Path,
    timeout_sec: f64,
    environment: &'a BTreeMap<String, String>,
    environment_mode: VerifierEnvironmentMode,
    collect: &'a [VerifierCollect],
}

#[derive(Serialize)]
struct ResourcesOutput {
    cpus: u32,
    memory_mb: u64,
    storage_mb: u64,
    gpus: u32,
}

impl<'a> From<&'a Task> for TaskOutput<'a> {
    fn from(task: &'a Task) -> Self {
        Self {
            name: task.name(),
            description: task.description(),
            root: task.root(),
            prompt: task.prompt(),
            image: task.image().reference(),
            agent_timeout_sec: task.agent_timeout().as_secs_f64(),
            verifier: VerifierOutput {
                script: task.verifier().script(),
                timeout_sec: task.verifier().timeout().as_secs_f64(),
                environment: task.verifier().environment(),
                environment_mode: task.verifier().environment_mode(),
                collect: task.verifier().collect(),
            },
            artifacts: task.artifacts(),
            resources: ResourcesOutput {
                cpus: task.resources().cpus,
                memory_mb: task.resources().memory_mb,
                storage_mb: task.resources().storage_mb,
                gpus: task.resources().gpus,
            },
            network: task.network().as_str(),
            environment: task.environment(),
            requires_compose: task.requires_compose(),
        }
    }
}

impl TaskOutput<'_> {
    fn write_human(&self, mut output: impl Write, include_prompt: bool) -> io::Result<()> {
        writeln!(output, "{}", self.name)?;
        writeln!(output, "  root: {}", self.root.display())?;
        writeln!(output, "  image: {}", self.image)?;
        writeln!(output, "  prompt: {} bytes", self.prompt.len())?;
        writeln!(
            output,
            "  timeout: {}s agent, {}s verifier",
            self.agent_timeout_sec, self.verifier.timeout_sec
        )?;
        writeln!(
            output,
            "  resources: {} CPU, {} MiB memory, {} MiB storage, {} GPU",
            self.resources.cpus,
            self.resources.memory_mb,
            self.resources.storage_mb,
            self.resources.gpus
        )?;
        writeln!(output, "  network: {}", self.network)?;
        writeln!(
            output,
            "  verifier: {} ({})",
            self.verifier.script.display(),
            self.verifier.environment_mode.as_str()
        )?;
        writeln!(output, "  artifacts: {}", self.artifacts.len())?;
        writeln!(output, "  requires compose: {}", self.requires_compose)?;
        if include_prompt {
            writeln!(output, "\n{}", self.prompt)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, str::FromStr};

    use clap::Parser;

    use super::{Cli, Command, ReadOnlyDisk, VmCommand, WritableShare};

    #[test]
    fn writable_share_is_a_strict_tag_and_directory_pair() {
        let share = WritableShare::from_str("nanoeval-cache=/tmp/cache").unwrap();
        assert_eq!(share.tag, "nanoeval-cache");
        assert_eq!(share.directory, Path::new("/tmp/cache"));

        assert!(WritableShare::from_str("missing-directory=").is_err());
        assert!(WritableShare::from_str("bad tag=/tmp/cache").is_err());
        assert!(WritableShare::from_str("/tmp/cache").is_err());
    }

    #[test]
    fn read_only_disk_uses_the_same_strict_pair_shape() {
        let disk = ReadOnlyDisk::from_str("runtime=/tmp/runtime.ext4").unwrap();
        assert_eq!(disk.id, "runtime");
        assert_eq!(disk.image, Path::new("/tmp/runtime.ext4"));
    }

    #[test]
    fn prepare_accepts_repeated_tasks_in_input_order() {
        let cli = Cli::try_parse_from([
            "nanoeval",
            "vm",
            "prepare",
            "--task",
            "tasks/first",
            "--task",
            "tasks/second",
        ])
        .unwrap();
        let Some(Command::Vm {
            command: VmCommand::Prepare { tasks, .. },
        }) = cli.command
        else {
            panic!("expected vm prepare command");
        };

        assert_eq!(
            tasks,
            [
                Path::new("tasks/first").to_path_buf(),
                Path::new("tasks/second").to_path_buf()
            ]
        );
    }

    #[test]
    fn prepare_accepts_a_complete_suite() {
        let cli =
            Cli::try_parse_from(["nanoeval", "vm", "prepare", "--suite", "terminal-bench-2-1"])
                .unwrap();
        let Some(Command::Vm {
            command: VmCommand::Prepare { suites, .. },
        }) = cli.command
        else {
            panic!("expected vm prepare command");
        };

        assert_eq!(suites, [Path::new("terminal-bench-2-1").to_path_buf()]);
    }

    #[test]
    fn vm_run_accepts_an_explicit_no_network_policy() {
        let cli = Cli::try_parse_from([
            "nanoeval",
            "vm",
            "run",
            "--root",
            "/tmp/rootfs.ext4",
            "--ext4",
            "--no-network",
            "/bin/true",
        ])
        .unwrap();
        let Some(Command::Vm {
            command: VmCommand::Run { no_network, .. },
        }) = cli.command
        else {
            panic!("expected vm run command");
        };

        assert!(no_network);
    }

    #[test]
    fn vm_run_rejects_conflicting_network_policies() {
        let parsed = Cli::try_parse_from([
            "nanoeval",
            "vm",
            "run",
            "--root",
            "/tmp/rootfs.ext4",
            "--network-socket",
            "/tmp/gvproxy.sock",
            "--no-network",
            "/bin/true",
        ]);

        assert!(parsed.is_err());
    }
}

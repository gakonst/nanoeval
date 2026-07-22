mod config;
mod eval;

use std::{
    collections::BTreeMap,
    io::{self, Write},
    path::{Path, PathBuf},
};

use clap::{CommandFactory, Parser, Subcommand};
use eyre::{Result, eyre};
use nanoeval::Task;
use nanovm::{GuestCommand, KrunVm, VmConfig};
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

    /// Run one command in a libkrun microVM.
    Vm {
        #[command(subcommand)]
        command: VmCommand,
    },
}

#[derive(Subcommand)]
enum VmCommand {
    /// Boot a root filesystem and replace Nanoeval with the guest command.
    Run {
        /// Linux root filesystem directory exposed to the guest.
        #[arg(long)]
        root: PathBuf,

        /// Number of virtual CPUs.
        #[arg(long, default_value_t = 2)]
        cpus: u8,

        /// Guest memory in MiB.
        #[arg(long, default_value_t = 1024)]
        memory_mib: u32,

        /// Executable and arguments to run inside the guest.
        #[arg(required = true, trailing_var_arg = true)]
        guest_command: Vec<std::ffi::OsString>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    run(Cli::parse()).await
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
        Command::Vm {
            command:
                VmCommand::Run {
                    root,
                    cpus,
                    memory_mib,
                    guest_command,
                },
        } => {
            let (program, arguments) = guest_command
                .split_first()
                .ok_or_else(|| eyre!("guest command must not be empty"))?;
            let config = VmConfig::new(root).cpus(cpus).memory_mib(memory_mib);
            let command = GuestCommand::new(program).args(arguments);
            KrunVm::new(&config)?.run(&command)?;
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
            },
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
        writeln!(output, "  verifier: {}", self.verifier.script.display())?;
        writeln!(output, "  requires compose: {}", self.requires_compose)?;
        if include_prompt {
            writeln!(output, "\n{}", self.prompt)?;
        }
        Ok(())
    }
}

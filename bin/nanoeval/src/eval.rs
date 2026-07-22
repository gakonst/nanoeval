use std::{
    fs, io,
    path::{Path, PathBuf},
};

use clap::Args;
use eyre::{Result, eyre};
use nanocodex::{Tools, ToolsBuildError, UpdatePlanTool};
use nanocodex_vm::{VmToolSession, VmToolSessionError, VmTools};
use nanoeval::{EvalAttempt, EvalEventKind, EvalResult, Nanoeval, NanoevalEventStream, Task};
use nanoeval_harbor::{Harbor, HarborJob};
use tokio::process::Command;

use crate::config::AgentArgs;

#[derive(Args)]
pub(crate) struct Eval {
    /// Terminal-Bench task directory. Repeat for multiple evals in one job.
    #[arg(long = "task", required = true, value_name = "DIRECTORY")]
    tasks: Vec<PathBuf>,

    /// Parent directory for the retained Harbor-compatible job.
    #[arg(long, default_value = "nanoeval-runs")]
    output: PathBuf,

    /// Number of fresh, independent attempts per task.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..))]
    trials: u16,

    /// Maximum number of attempts executing at once.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..))]
    concurrency: u16,

    /// Print typed results as JSON instead of a human summary.
    #[arg(long)]
    json: bool,

    /// Alpine/libkrun rootfs template. Enables VM-owned workspace tools.
    #[arg(long, value_name = "DIRECTORY")]
    vm_rootfs: Option<PathBuf>,

    #[command(flatten)]
    agent: AgentArgs,
}

impl Eval {
    pub(crate) async fn run(self) -> Result<()> {
        let trials = usize::from(self.trials);
        let tasks = self
            .tasks
            .into_iter()
            .map(Task::load)
            .collect::<Result<Vec<_>, _>>()?;
        let attempt_count = tasks.len() * trials;
        let attempts = tasks
            .into_iter()
            .flat_map(|task| std::iter::repeat_n(task, trials));
        let mut evaluator = Nanoeval::builder(self.agent.builder()?)
            .output_directory(self.output)
            .max_concurrency(usize::from(self.concurrency));
        if let Some(rootfs) = self.vm_rootfs {
            let vmm = std::env::current_exe()?;
            evaluator = evaluator.attempt_agent(move |attempt, builder| {
                let tools = vm_attempt_tools(&rootfs, &vmm, attempt)?;
                Ok::<_, VmAttemptError>(builder.tools(tools))
            });
        }
        let (eval, events) = evaluator.build()?;
        let harbor = Harbor::new(&eval)?.record(events.subscribe())?;
        let progress = tokio::spawn(report_progress(events.subscribe(), attempt_count));
        let results = eval.tasks(attempts).await?;
        let job = harbor.finish(results.clone()).await?;
        progress.await??;
        if self.json {
            serde_json::to_writer_pretty(io::stdout().lock(), &results)?;
            println!();
        } else {
            Self::write_summary(&job, &results);
        }
        Ok(())
    }

    fn write_summary(job: &HarborJob, results: &[EvalResult]) {
        for result in results {
            println!("{}: {:?}", result.trial_name, result.status);
        }
        println!("Harbor job: {}", job.directory().display());
    }
}

const GUEST_TOOL_RUNTIME: &str = "/usr/local/bin/nanocodex-vm-guest";

#[derive(Debug, thiserror::Error)]
enum VmAttemptError {
    #[error("rootfs template is not a directory: {0}")]
    InvalidRootfs(PathBuf),

    #[error("rootfs template does not contain the guest tool runtime: {0}")]
    MissingGuestRuntime(PathBuf),

    #[error("rootfs entry collides with attempt data: {0}")]
    Collision(PathBuf),

    #[error(transparent)]
    Io(#[from] io::Error),

    #[error(transparent)]
    Session(#[from] VmToolSessionError),

    #[error(transparent)]
    Tools(#[from] ToolsBuildError),
}

fn vm_attempt_tools(
    template: &Path,
    vmm: &Path,
    attempt: EvalAttempt<'_>,
) -> Result<Tools, VmAttemptError> {
    let guest_runtime = template.join(GUEST_TOOL_RUNTIME.trim_start_matches('/'));
    if !guest_runtime.is_file() {
        return Err(VmAttemptError::MissingGuestRuntime(guest_runtime));
    }
    materialize_rootfs(template, attempt.directory())?;
    let cpus = attempt.task().resources().cpus.clamp(1, u32::from(u8::MAX));
    let memory_mib = attempt
        .task()
        .resources()
        .memory_mb
        .clamp(1, u64::from(u32::MAX));
    let mut command = Command::new(vmm);
    command
        .arg("vm")
        .arg("run")
        .arg("--root")
        .arg(attempt.directory())
        .arg("--cpus")
        .arg(cpus.to_string())
        .arg("--memory-mib")
        .arg(memory_mib.to_string())
        .arg(GUEST_TOOL_RUNTIME)
        .arg("/workspace");
    let vm = VmTools::new(VmToolSession::spawn(&mut command)?);
    Tools::builder()
        .without_defaults()
        .web_search(true)
        .image_generation(true)
        .working_directory("/workspace")
        .default_shell("sh")
        .tool(vm.exec_command_tool())
        .tool(vm.write_stdin_tool())
        .tool(vm.apply_patch_tool())
        .tool(vm.view_image_tool())
        .tool(UpdatePlanTool::new())
        .build()
        .map_err(Into::into)
}

fn materialize_rootfs(source: &Path, destination: &Path) -> Result<(), VmAttemptError> {
    if !source.is_dir() {
        return Err(VmAttemptError::InvalidRootfs(source.to_path_buf()));
    }
    copy_root_entries(source, destination, true)
}

fn copy_root_entries(source: &Path, destination: &Path, root: bool) -> Result<(), VmAttemptError> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if root && matches!(entry.file_name().to_str(), Some("workspace" | "verifier")) {
            continue;
        }
        let source = entry.path();
        let target = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source)?;
        if metadata.file_type().is_symlink() {
            if target.exists() || fs::symlink_metadata(&target).is_ok() {
                return Err(VmAttemptError::Collision(target));
            }
            std::os::unix::fs::symlink(fs::read_link(source)?, target)?;
        } else if metadata.is_dir() {
            if target.exists() && !target.is_dir() {
                return Err(VmAttemptError::Collision(target));
            }
            fs::create_dir_all(&target)?;
            copy_root_entries(&source, &target, false)?;
        } else if metadata.is_file() {
            if target.exists() {
                return Err(VmAttemptError::Collision(target));
            }
            fs::copy(source, target)?;
        } else {
            return Err(VmAttemptError::Collision(source));
        }
    }
    Ok(())
}

async fn report_progress(mut events: NanoevalEventStream, expected: usize) -> Result<()> {
    let mut completed = 0;
    while completed < expected {
        let event = events
            .recv()
            .await?
            .ok_or_else(|| eyre!("event stream closed after {completed} of {expected} attempts"))?;
        match &event.kind {
            EvalEventKind::AttemptStarted { .. } => {
                eprintln!("{}: started", event.trial_name);
            }
            EvalEventKind::Completed(result) => {
                completed += 1;
                eprintln!("{}: {:?}", event.trial_name, result.status);
            }
            EvalEventKind::Agent(_)
            | EvalEventKind::VerifierStarted
            | EvalEventKind::VerifierOutput { .. }
            | EvalEventKind::VerifierCompleted(_) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::Parser;

    use super::Eval;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        eval: Eval,
    }

    #[test]
    fn accepts_repeated_tasks_with_per_task_trials() {
        let cli = TestCli::try_parse_from([
            "nanoeval",
            "--task",
            "tasks/first",
            "--task",
            "tasks/second",
            "--trials",
            "5",
            "--concurrency",
            "10",
        ])
        .unwrap();

        assert_eq!(
            cli.eval.tasks,
            [PathBuf::from("tasks/first"), PathBuf::from("tasks/second")]
        );
        assert_eq!(cli.eval.trials, 5);
        assert_eq!(cli.eval.concurrency, 10);
    }

    #[test]
    fn requires_at_least_one_task() {
        let Err(error) = TestCli::try_parse_from(["nanoeval"]) else {
            panic!("a task should be required");
        };
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }
}

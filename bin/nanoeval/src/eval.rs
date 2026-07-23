use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fs,
    future::Future,
    io::{self, Write},
    num::ParseFloatError,
    path::{Path, PathBuf},
    pin::Pin,
    str::FromStr,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use arcbox_ext4::{
    Formatter, Reader,
    constants::{file_mode, make_mode},
};
use chrono::{DateTime, Utc};
use clap::{Args, ValueEnum};
use eyre::{Result, eyre};
use nanocodex::{Thinking, Tools, ToolsBuildError, UpdatePlanTool};
use nanocodex_vm::{VmCommand, VmCommandOutput, VmToolSession, VmToolSessionError, VmTools};
use nanoeval::{
    AttemptAgent, AttemptVerification, AttemptVerifier, EvalAttempt, EvalEventKind, EvalFailure,
    EvalFailureKind, EvalResult, EvalStatus, Nanoeval, NanoevalBuilder, NanoevalEventStream, Sweep,
    SweepResults, Task, VerifierResult,
};
use nanoeval_harbor::{Harbor, HarborJob, HarborRecorder};
use regex::RegexSet;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sysinfo::{MemoryRefreshKind, RefreshKind, System};
use tokio::process::Command;
use tracing::{info, info_span, warn};
use yansi::Painted;

use crate::config::AgentArgs;
use crate::observability::ObservabilityArgs;
use crate::vm_image::{CachePolicy, PreparedRootDisk, VmImageBuilder};
use crate::vm_network::{Gvproxy, GvproxyError, prepare_gvproxy};

const DEFAULT_OUTPUT_DIRECTORY: &str = "nanoeval-runs";
const INVOCATION_FILE: &str = "invocation.json";
const LAST_RUN_FILE: &str = ".nanoeval/last-run.json";
const INVOCATION_VERSION: u32 = 1;
const DEFAULT_HOST_UTILIZATION_PERCENT: u8 = 80;
const BYTES_PER_MIB: u64 = 1024 * 1024;

#[derive(Args)]
#[group(id = "task_input", required = true, multiple = true)]
pub(crate) struct Eval {
    #[command(flatten)]
    observability: ObservabilityArgs,

    /// Terminal-Bench task directory. Repeat for multiple evals in one job.
    #[arg(long = "task", value_name = "DIRECTORY", group = "task_input")]
    tasks: Vec<PathBuf>,

    /// Terminal-Bench suite directory whose immediate task children should run.
    #[arg(long = "suite", value_name = "DIRECTORY", group = "task_input")]
    suites: Vec<PathBuf>,

    #[command(flatten)]
    retry: RetryArgs,

    /// Parent directory for the retained Harbor-compatible job.
    #[arg(long)]
    output: Option<PathBuf>,

    /// Number of fresh, independent attempts per task.
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    trials: Option<u16>,

    /// Maximum number of attempts executing at once.
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    concurrency: Option<u16>,

    /// Maximum sum of task-declared memory across concurrent attempts.
    #[arg(long, value_name = "MIB", value_parser = clap::value_parser!(u64).range(1..))]
    max_memory_mb: Option<u64>,

    /// Percentage of detected host CPU and memory used for omitted scheduler limits.
    #[arg(
        long,
        default_value_t = DEFAULT_HOST_UTILIZATION_PERCENT,
        value_name = "PERCENT",
        value_parser = clap::value_parser!(u8).range(1..=100)
    )]
    host_utilization: u8,

    #[command(flatten)]
    lifecycle: RunLifecycleArgs,

    /// Print typed results as JSON instead of a human summary.
    #[arg(long)]
    json: bool,

    /// Run workspace tools inside a libkrun microVM.
    #[arg(long)]
    vm: bool,

    /// Override the prepared rootfs directory or raw ext4 image used by `--vm`.
    #[arg(long, value_name = "PATH")]
    vm_rootfs: Option<PathBuf>,

    /// Resolve the task image at the registry instead of reusing its local resolution.
    #[arg(long, requires = "vm", conflicts_with = "vm_rootfs")]
    vm_refresh: bool,

    /// Writable VM root-disk retention policy.
    #[arg(long, value_enum)]
    vm_retention: Option<VmRetention>,

    #[command(flatten)]
    agent: AgentArgs,
}

#[derive(Args)]
struct RetryArgs {
    /// Rerun unresolved tasks from the latest completed Nanoeval job.
    #[arg(long, group = "task_input", conflicts_with_all = ["tasks", "suites"])]
    rerun: bool,

    /// Literal task-name substring to rerun. Repeat positional values for OR matching.
    #[arg(value_name = "NAME", requires = "rerun")]
    names: Vec<String>,

    /// Resolve the retry queue from this job instead of the latest completed job.
    #[arg(long, value_name = "JOB", requires = "rerun")]
    rerun_from: Option<PathBuf>,

    /// Advanced regular expression over full task names. Repeat for OR matching.
    #[arg(long, value_name = "REGEX", requires = "rerun")]
    match_task: Vec<String>,

    /// Print the selected task names without starting a new evaluation job.
    #[arg(long, requires = "rerun")]
    list: bool,

    #[command(flatten)]
    statuses: RetryStatusArgs,
}

#[derive(Args)]
struct RetryStatusArgs {
    /// Include typed safety refusals in the rerun selection.
    #[arg(long, requires = "rerun")]
    include_refused: bool,

    /// Include harness-errored tasks in the rerun selection.
    #[arg(long, requires = "rerun")]
    include_errored: bool,
}

#[derive(Args)]
struct RunLifecycleArgs {
    /// Start a new job even when a matching incomplete job can be resumed.
    #[arg(long = "new")]
    new_job: bool,
}

#[derive(Clone, Debug)]
struct ResolvedRun {
    task_paths: Vec<PathBuf>,
    output: PathBuf,
    trials: u16,
    concurrency: u16,
    max_memory_mb: Option<u64>,
    vm: bool,
    vm_rootfs: Option<PathBuf>,
    vm_retention: VmRetention,
    thinking: Thinking,
    rerun_from: Option<PathBuf>,
    automatic_scheduling: Option<AutomaticScheduling>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HostResources {
    logical_cpus: usize,
    physical_memory_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SchedulingDefaults {
    concurrency: u16,
    max_memory_mb: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResolvedScheduling {
    concurrency: u16,
    max_memory_mb: Option<u64>,
    automatic: Option<AutomaticScheduling>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AutomaticScheduling {
    utilization_percent: u8,
    host: HostResources,
    concurrency: bool,
    memory: bool,
}

struct RerunSelection {
    job: PathBuf,
    tasks: Vec<PathBuf>,
}

struct RetainedRetryQueue {
    task_names: BTreeSet<String>,
    unresolved_tasks: usize,
    lineage: Vec<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RunInvocation {
    version: u32,
    trials: u16,
    concurrency: u16,
    max_memory_mb: Option<u64>,
    vm: bool,
    vm_rootfs: Option<PathBuf>,
    vm_retention: VmRetention,
    thinking: String,
    rerun_from: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LastRun {
    job: PathBuf,
}

#[derive(Debug, Deserialize)]
struct RetainedRun {
    tasks: Vec<RetainedRunTask>,
}

#[derive(Debug, Deserialize)]
struct RetainedRunTask {
    root: PathBuf,
}

#[derive(Debug, Deserialize)]
struct RetainedJobIdentity {
    started_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct RetainedTrialResult {
    task_name: String,
    verifier_result: Option<RetainedVerifierResult>,
    exception_info: Option<RetainedTrialException>,
}

#[derive(Debug, Deserialize)]
struct RetainedVerifierResult {
    rewards: BTreeMap<String, f64>,
}

#[derive(Debug, Deserialize)]
struct RetainedTrialException {
    exception_type: String,
}

#[derive(Debug, Deserialize)]
struct LegacyJobConfig {
    n_concurrent_trials: usize,
    agents: Vec<LegacyAgentConfig>,
}

#[derive(Debug, Deserialize)]
struct LegacyAgentConfig {
    kwargs: LegacyAgentKwargs,
}

#[derive(Debug, Deserialize)]
struct LegacyAgentKwargs {
    effort: String,
}

impl HostResources {
    fn detect() -> Self {
        let logical_cpus = std::thread::available_parallelism().map_or(1, usize::from);
        let system = System::new_with_specifics(
            RefreshKind::nothing().with_memory(MemoryRefreshKind::nothing().with_ram()),
        );
        let physical_memory_bytes = match system.total_memory() {
            0 => None,
            bytes => Some(bytes),
        };
        Self {
            logical_cpus,
            physical_memory_bytes,
        }
    }

    fn scheduling_defaults(self, utilization_percent: u8) -> SchedulingDefaults {
        let logical_cpus = u64::try_from(self.logical_cpus).unwrap_or(u64::MAX);
        let concurrency = percentage(logical_cpus, utilization_percent).max(1);
        let concurrency = u16::try_from(concurrency).unwrap_or(u16::MAX);
        let max_memory_mb = self
            .physical_memory_bytes
            .map(|bytes| (percentage(bytes, utilization_percent) / BYTES_PER_MIB).max(1));
        SchedulingDefaults {
            concurrency,
            max_memory_mb,
        }
    }
}

const fn percentage(value: u64, percent: u8) -> u64 {
    value.saturating_mul(percent as u64) / 100
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RetainedTrialStatus {
    Passed,
    Failed,
    Refused,
    Errored,
}

impl Eval {
    fn resolve_scheduling(
        &self,
        retained: Option<&RunInvocation>,
        legacy: Option<&LegacyJobConfig>,
    ) -> ResolvedScheduling {
        let host = HostResources::detect();
        let defaults = host.scheduling_defaults(self.host_utilization);
        let retained_concurrency = retained.map(|invocation| invocation.concurrency);
        let legacy_concurrency = legacy.and_then(|job| u16::try_from(job.n_concurrent_trials).ok());
        let automatic_concurrency = self.concurrency.is_none()
            && retained_concurrency.is_none()
            && legacy_concurrency.is_none();
        let concurrency = self
            .concurrency
            .or(retained_concurrency)
            .or(legacy_concurrency)
            .unwrap_or(defaults.concurrency);
        let (max_memory_mb, automatic_memory) = if let Some(memory) = self.max_memory_mb {
            (Some(memory), false)
        } else if let Some(invocation) = retained {
            (invocation.max_memory_mb, false)
        } else if legacy.is_some() {
            (None, false)
        } else {
            (defaults.max_memory_mb, defaults.max_memory_mb.is_some())
        };
        let automatic =
            (automatic_concurrency || automatic_memory).then_some(AutomaticScheduling {
                utilization_percent: self.host_utilization,
                host,
                concurrency: automatic_concurrency,
                memory: automatic_memory,
            });
        ResolvedScheduling {
            concurrency,
            max_memory_mb,
            automatic,
        }
    }

    fn resolve_run(&self) -> Result<ResolvedRun> {
        let rerun = self
            .retry
            .rerun
            .then(|| resolve_rerun_source(self))
            .transpose()?;
        let retained_invocation = match &rerun {
            Some(rerun) => load_invocation(&rerun.job)?,
            None => None,
        };
        let legacy = rerun
            .as_ref()
            .map(|rerun| load_legacy_job_config(&rerun.job))
            .transpose()?;
        let task_paths = match &rerun {
            Some(rerun) => rerun.tasks.clone(),
            None => load_task_paths(self.tasks.clone(), self.suites.clone())?,
        };
        let output = self.output.clone().unwrap_or_else(|| {
            rerun
                .as_ref()
                .and_then(|rerun| rerun.job.parent())
                .map_or_else(
                    || PathBuf::from(DEFAULT_OUTPUT_DIRECTORY),
                    Path::to_path_buf,
                )
        });
        let retained_thinking = retained_invocation
            .as_ref()
            .map(|invocation| {
                Thinking::from_str(&invocation.thinking).map_err(|error| {
                    eyre!(
                        "invalid thinking level {:?} in {INVOCATION_FILE}: {error}",
                        invocation.thinking
                    )
                })
            })
            .transpose()?;
        let legacy_thinking = legacy.as_ref().map(LegacyJobConfig::thinking).transpose()?;
        let thinking = self
            .agent
            .thinking()
            .or(retained_thinking)
            .or(legacy_thinking)
            .unwrap_or_default();
        let vm_rootfs = self.vm_rootfs.clone().or_else(|| {
            retained_invocation
                .as_ref()
                .and_then(|invocation| invocation.vm_rootfs.clone())
        });
        let vm = self.vm
            || retained_invocation
                .as_ref()
                .is_some_and(|invocation| invocation.vm)
            || rerun
                .as_ref()
                .is_some_and(|rerun| retained_job_used_vm(&rerun.job));
        let scheduling = self.resolve_scheduling(retained_invocation.as_ref(), legacy.as_ref());
        Ok(ResolvedRun {
            task_paths,
            output,
            trials: self.trials.unwrap_or(1),
            concurrency: scheduling.concurrency,
            max_memory_mb: scheduling.max_memory_mb,
            vm,
            vm_rootfs,
            vm_retention: self
                .vm_retention
                .or_else(|| {
                    retained_invocation
                        .as_ref()
                        .map(|invocation| invocation.vm_retention)
                })
                .unwrap_or_default(),
            thinking,
            rerun_from: rerun.map(|rerun| rerun.job),
            automatic_scheduling: scheduling.automatic,
        })
    }

    fn resolve_executable_run(&self) -> Result<Option<ResolvedRun>> {
        let resolved = self.resolve_run()?;
        if self.retry.list {
            write_task_names(&resolved.task_paths, self.json)?;
            return Ok(None);
        }
        resolved.report_automatic_scheduling();
        Ok(Some(resolved))
    }

    pub(crate) async fn run(self) -> Result<()> {
        let total_started = Instant::now();
        let Some(resolved) = self.resolve_executable_run()? else {
            return Ok(());
        };
        let observability_started = Instant::now();
        let _observability = self.observability.install()?;
        let observability = observability_started.elapsed();
        let (tasks, task_loading) =
            load_prioritized_tasks(resolved.task_paths.clone(), &resolved.output)?;
        let (vmm, runtime_image, vm_runtime) =
            prepare_run_vm(resolved.vm, resolved.vm_rootfs.as_deref()).await?;
        let gvproxy = prepare_network_for_vm(resolved.vm || resolved.vm_rootfs.is_some()).await?;
        let vm_environments_started = Instant::now();
        let vm_environments = selected_vm_environments(
            &tasks,
            resolved.vm,
            resolved.vm_rootfs.clone(),
            self.vm_refresh,
            &vmm,
            &runtime_image,
        )
        .await?;
        let vm_environments_duration = vm_environments_started.elapsed();
        let evaluation_setup_started = Instant::now();
        let nanocodex = self.agent.builder(resolved.thinking)?;
        let sweep = Sweep::builder()
            .tasks(tasks)
            .trials(resolved.trials)
            .agent("default", nanocodex.clone())?
            .build()?;
        let attempt_count = sweep.attempt_count();
        let evaluator = Nanoeval::builder(nanocodex)
            .output_directory(&resolved.output)
            .max_concurrency(usize::from(resolved.concurrency));
        let evaluator = configure_memory_limit(evaluator, resolved.max_memory_mb);
        let mut evaluator = bind_finite_run(evaluator, &sweep, self.lifecycle.new_job);
        if let Some(environments) = vm_environments {
            let gvproxy = gvproxy.ok_or_else(|| eyre!("VM network backend was not prepared"))?;
            evaluator = evaluator.attempt_agent(move |attempt, builder| {
                let environment = environments.get(attempt.task().root()).ok_or_else(|| {
                    VmAttemptError::MissingPreparedEnvironment(attempt.task().root().to_path_buf())
                })?;
                let runtime = vm_attempt(
                    environment,
                    VmAttemptHost {
                        runtime_image: &runtime_image,
                        vmm: &vmm,
                        gvproxy: &gvproxy,
                        retain_passed_rootfs: resolved.vm_retention.retains_passes(),
                    },
                    attempt,
                )?;
                Ok::<_, VmAttemptError>(
                    AttemptAgent::new(builder.tools(runtime.tools)).verifier(runtime.verifier),
                )
            });
        }
        let (eval, events) = evaluator.build()?;
        persist_invocation(eval.directory(), &resolved.invocation())?;
        let remaining_attempts = eval.remaining_attempts(&sweep)?;
        let skipped_attempts = attempt_count.saturating_sub(remaining_attempts);
        report_resume(&eval, skipped_attempts, attempt_count);
        let harbor = Harbor::new(&eval)?.record(events.subscribe())?;
        let progress = tokio::spawn(report_progress(
            events.subscribe(),
            remaining_attempts,
            usize::from(resolved.concurrency),
            resolved.max_memory_mb,
        ));
        let evaluation_setup = evaluation_setup_started.elapsed();
        let attempts_started = Instant::now();
        let sweep_result = eval.sweep(sweep).await;
        let attempts = attempts_started.elapsed();
        let finished =
            finish_evaluation(harbor, remaining_attempts, progress, sweep_result).await?;
        let output_started = Instant::now();
        Self::write_report(
            &finished.job,
            finished.outcomes,
            skipped_attempts,
            self.json,
        )?;
        let output = output_started.elapsed();
        RunMeasurements {
            observability,
            task_loading,
            vm_runtime,
            vm_environments: vm_environments_duration,
            evaluation_setup,
            attempts,
            harbor_finish: finished.harbor_finish,
            output,
            total: total_started.elapsed(),
        }
        .record(&finished.results, attempt_count, finished.failed);
        record_last_run(finished.job.directory())?;
        finish_run(finished.run_error)
    }

    fn write_report(
        job: &HarborJob,
        outcomes: Vec<AttemptOutcome>,
        skipped: usize,
        json: bool,
    ) -> Result<()> {
        let report = RunReport::new(job, outcomes, skipped);
        if json {
            serde_json::to_writer_pretty(io::stdout().lock(), &report)?;
            println!();
        } else {
            Self::write_summary(&report);
        }
        Ok(())
    }

    fn write_summary(report: &RunReport) {
        println!(
            "\nResult: {} passed; {} failed; {} refused; {} errored; {} total",
            Painted::new(report.summary.passed).green(),
            Painted::new(report.summary.failed).red(),
            Painted::new(report.summary.refused).yellow(),
            Painted::new(report.summary.errored).red(),
            report.summary.total
        );
        println!("Harbor job: {}", report.job_directory.display());
        if report.skipped > 0 {
            println!(
                "Resumed: {} previously completed attempt{} retained",
                report.skipped,
                if report.skipped == 1 { "" } else { "s" }
            );
        }
        if report.summary.failed + report.summary.refused + report.summary.errored > 0 {
            println!(
                "Inspect failures: nanoeval inspect {}",
                report.job_directory.display()
            );
        }
    }
}

impl ResolvedRun {
    fn report_automatic_scheduling(&self) {
        let Some(automatic) = self.automatic_scheduling else {
            return;
        };
        let memory = automatic.host.physical_memory_bytes.map_or_else(
            || "unknown RAM".to_owned(),
            |bytes| format!("{} MiB RAM", bytes / BYTES_PER_MIB),
        );
        let concurrency_source = if automatic.concurrency {
            "automatic"
        } else {
            "configured"
        };
        let memory_source = if automatic.memory {
            "automatic"
        } else {
            "configured"
        };
        let max_memory = self
            .max_memory_mb
            .map_or_else(|| "unbounded".to_owned(), |memory| format!("{memory} MiB"));
        eprintln!(
            "Host scheduling: target={}%, detected={} logical CPUs/{memory}, \
             concurrency={} ({concurrency_source}), memory={max_memory} ({memory_source})",
            automatic.utilization_percent, automatic.host.logical_cpus, self.concurrency,
        );
    }

    fn invocation(&self) -> RunInvocation {
        RunInvocation {
            version: INVOCATION_VERSION,
            trials: self.trials,
            concurrency: self.concurrency,
            max_memory_mb: self.max_memory_mb,
            vm: self.vm,
            vm_rootfs: self.vm_rootfs.clone(),
            vm_retention: self.vm_retention,
            thinking: self.thinking.to_string(),
            rerun_from: self.rerun_from.clone(),
        }
    }
}

impl LegacyJobConfig {
    fn thinking(&self) -> Result<Thinking> {
        let effort = self
            .agents
            .first()
            .ok_or_else(|| eyre!("retained job config contains no agent"))?
            .kwargs
            .effort
            .as_str();
        Thinking::from_str(effort).map_err(|error| eyre!(error))
    }
}

fn resolve_rerun_source(eval: &Eval) -> Result<RerunSelection> {
    let job = match &eval.retry.rerun_from {
        Some(job) => resolve_job_path(job, eval.output.as_deref())?,
        None => latest_completed_job(eval.output.as_deref())?,
    };
    if !job.join("result.json").is_file() {
        return Err(eyre!(
            "rerun source is not a completed Nanoeval job: {}",
            job.display()
        ));
    }
    let matcher = retry_matcher(&eval.retry)?;
    let queue = retained_retry_task_names(
        &job,
        eval.retry.statuses.include_refused,
        eval.retry.statuses.include_errored,
        matcher.as_ref(),
    )?;
    let tasks = retained_retry_task_roots(&queue.lineage, &queue.task_names)?;
    if tasks.is_empty() {
        let filter = if eval.retry.match_task.is_empty() && eval.retry.names.is_empty() {
            String::new()
        } else {
            format!(
                " matching names {:?} or regular expressions {:?}",
                eval.retry.names, eval.retry.match_task
            )
        };
        return Err(eyre!(
            "no unresolved tasks{filter}; inspect the queue with `nanoeval run --rerun --list`"
        ));
    }
    eprintln!(
        "{}",
        retry_selection_summary(eval, &queue, &job, tasks.len())
    );
    if !eval.retry.list && !eval.json {
        for task in &tasks {
            eprintln!("  {}", short_task_name(Task::load(task)?.name()));
        }
    }
    Ok(RerunSelection { job, tasks })
}

fn retry_matcher(retry: &RetryArgs) -> Result<Option<RegexSet>> {
    let mut patterns = retry.match_task.clone();
    patterns.extend(retry.names.iter().map(|name| regex::escape(name)));
    (!patterns.is_empty())
        .then(|| RegexSet::new(patterns))
        .transpose()
        .map_err(Into::into)
}

fn retry_selection_summary(
    eval: &Eval,
    queue: &RetainedRetryQueue,
    job: &Path,
    selected: usize,
) -> String {
    let run = if queue.lineage.len() == 1 {
        "run"
    } else {
        "runs"
    };
    let job = job
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("<job>");
    if eval.retry.list {
        if selected == queue.unresolved_tasks {
            format!(
                "{} unresolved task{} across {} {run} (latest {job})",
                queue.unresolved_tasks,
                if queue.unresolved_tasks == 1 { "" } else { "s" },
                queue.lineage.len()
            )
        } else {
            format!(
                "{selected} selected of {} unresolved tasks across {} {run} (latest {job})",
                queue.unresolved_tasks,
                queue.lineage.len()
            )
        }
    } else {
        format!(
            "Retrying {selected} of {} unresolved task{} across {} {run} (latest {job})",
            queue.unresolved_tasks,
            if queue.unresolved_tasks == 1 { "" } else { "s" },
            queue.lineage.len()
        )
    }
}

fn write_task_names(tasks: &[PathBuf], json: bool) -> Result<()> {
    let names = tasks
        .iter()
        .map(|task| Task::load(task).map(|task| task.name().to_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    if json {
        serde_json::to_writer_pretty(io::stdout().lock(), &names)?;
        println!();
    } else {
        for name in names {
            println!("{}", short_task_name(&name));
        }
    }
    Ok(())
}

fn short_task_name(name: &str) -> &str {
    name.rsplit_once('/').map_or(name, |(_, name)| name)
}

fn latest_completed_job(output: Option<&Path>) -> Result<PathBuf> {
    if let Ok(retained) = read_json::<LastRun>(Path::new(LAST_RUN_FILE))
        && let Ok(job) = resolve_job_path(&retained.job, output)
        && job.join("result.json").is_file()
    {
        return Ok(job);
    }
    let current = std::env::current_dir()?;
    let mut roots = vec![output.map_or_else(|| current.clone(), Path::to_path_buf)];
    if output.is_none() {
        roots.extend(
            fs::read_dir(&current)?
                .filter_map(Result::ok)
                .filter_map(|entry| entry.file_type().ok()?.is_dir().then_some(entry.path())),
        );
    }
    let mut candidates = Vec::new();
    for root in roots {
        collect_completed_job(&root, &mut candidates);
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                collect_completed_job(&entry.path(), &mut candidates);
            }
        }
    }
    candidates.sort_unstable_by_key(|(started_at, _)| *started_at);
    candidates.pop().map(|(_, job)| job).ok_or_else(|| {
        eyre!("no completed Nanoeval job was found; run an eval or pass --rerun-from <JOB>")
    })
}

fn collect_completed_job(directory: &Path, candidates: &mut Vec<(DateTime<Utc>, PathBuf)>) {
    if !directory.join("result.json").is_file() || !directory.join("run.json").is_file() {
        return;
    }
    let Ok(identity) = read_json::<RetainedJobIdentity>(&directory.join("job.json")) else {
        return;
    };
    let Ok(directory) = fs::canonicalize(directory) else {
        return;
    };
    candidates.push((identity.started_at, directory));
}

fn resolve_job_path(job: &Path, output: Option<&Path>) -> Result<PathBuf> {
    let candidate = if job.is_dir() {
        job.to_path_buf()
    } else if job.components().count() == 1 {
        output
            .unwrap_or_else(|| Path::new(DEFAULT_OUTPUT_DIRECTORY))
            .join(job)
    } else {
        job.to_path_buf()
    };
    fs::canonicalize(&candidate).map_err(|error| {
        eyre!(
            "Nanoeval job does not exist: {}: {error}",
            candidate.display()
        )
    })
}

fn retained_retry_task_names(
    job: &Path,
    include_refused: bool,
    include_errored: bool,
    matcher: Option<&RegexSet>,
) -> Result<RetainedRetryQueue> {
    let lineage = retained_retry_lineage(job)?;
    let mut statuses = BTreeMap::new();
    for job in &lineage {
        for (task_name, status) in retained_task_statuses(job)? {
            statuses.insert(task_name, status);
        }
    }
    let retryable_names = statuses
        .into_iter()
        .filter_map(|(task_name, status)| {
            let retryable = match status {
                RetainedTrialStatus::Failed => true,
                RetainedTrialStatus::Refused => include_refused,
                RetainedTrialStatus::Errored => include_errored,
                RetainedTrialStatus::Passed => false,
            };
            retryable.then_some(task_name)
        })
        .collect::<BTreeSet<_>>();
    let unresolved_tasks = retryable_names.len();
    let task_names = retryable_names
        .into_iter()
        .filter(|task_name| matcher.is_none_or(|matcher| matcher.is_match(task_name)))
        .collect();
    Ok(RetainedRetryQueue {
        task_names,
        unresolved_tasks,
        lineage,
    })
}

fn retained_retry_task_roots(
    lineage: &[PathBuf],
    selected_names: &BTreeSet<String>,
) -> Result<Vec<PathBuf>> {
    let mut roots = BTreeMap::new();
    for job in lineage {
        let retained: RetainedRun = read_json(&job.join("run.json"))?;
        for retained_task in retained.tasks {
            let task = Task::load(&retained_task.root)?;
            if selected_names.contains(task.name()) {
                roots.insert(task.name().to_owned(), retained_task.root);
            }
        }
    }
    let missing = selected_names
        .iter()
        .filter(|name| !roots.contains_key(*name))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(eyre!(
            "retry lineage does not retain task definitions for {}",
            missing.join(", ")
        ));
    }
    Ok(roots.into_values().collect())
}

fn retained_retry_lineage(job: &Path) -> Result<Vec<PathBuf>> {
    let mut current = fs::canonicalize(job)?;
    let mut seen = BTreeSet::new();
    let mut lineage = Vec::new();
    loop {
        if !seen.insert(current.clone()) {
            return Err(eyre!(
                "retry lineage contains a cycle at {}",
                current.display()
            ));
        }
        lineage.push(current.clone());
        let Some(parent) = load_invocation(&current)?.and_then(|invocation| invocation.rerun_from)
        else {
            break;
        };
        current = fs::canonicalize(&parent).map_err(|error| {
            eyre!(
                "retry parent {} recorded by {} is unavailable: {error}",
                parent.display(),
                current.join(INVOCATION_FILE).display()
            )
        })?;
    }
    lineage.reverse();
    Ok(lineage)
}

fn retained_task_statuses(job: &Path) -> Result<BTreeMap<String, RetainedTrialStatus>> {
    let mut statuses: BTreeMap<String, RetainedTrialStatus> = BTreeMap::new();
    for entry in fs::read_dir(job)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let result_path = entry.path().join("result.json");
        if !result_path.is_file() {
            continue;
        }
        let result: RetainedTrialResult = read_json(&result_path)?;
        let status = result.status();
        statuses
            .entry(result.task_name)
            .and_modify(|retained| *retained = retained.merge(status))
            .or_insert(status);
    }
    Ok(statuses)
}

impl RetainedTrialResult {
    fn status(&self) -> RetainedTrialStatus {
        if let Some(exception) = &self.exception_info {
            return if exception.exception_type == "AgentSafetyRefusalError" {
                RetainedTrialStatus::Refused
            } else {
                RetainedTrialStatus::Errored
            };
        }
        if self
            .verifier_result
            .as_ref()
            .is_some_and(|verifier| verifier.rewards.values().all(|reward| *reward > 0.0))
        {
            RetainedTrialStatus::Passed
        } else {
            RetainedTrialStatus::Failed
        }
    }
}

impl RetainedTrialStatus {
    const fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Failed, _) | (_, Self::Failed) => Self::Failed,
            (Self::Errored, _) | (_, Self::Errored) => Self::Errored,
            (Self::Refused, _) | (_, Self::Refused) => Self::Refused,
            (Self::Passed, Self::Passed) => Self::Passed,
        }
    }
}

fn load_invocation(job: &Path) -> Result<Option<RunInvocation>> {
    let path = job.join(INVOCATION_FILE);
    match fs::read(&path) {
        Ok(contents) => {
            let invocation: RunInvocation = serde_json::from_slice(&contents)?;
            if invocation.version != INVOCATION_VERSION {
                return Err(eyre!(
                    "unsupported Nanoeval invocation version {} in {}",
                    invocation.version,
                    path.display()
                ));
            }
            Ok(Some(invocation))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn load_legacy_job_config(job: &Path) -> Result<LegacyJobConfig> {
    read_json(&job.join("config.json"))
}

fn retained_job_used_vm(job: &Path) -> bool {
    fs::read_dir(job).is_ok_and(|entries| {
        entries
            .filter_map(Result::ok)
            .any(|entry| entry.path().join("rootfs.ext4").is_file())
    })
}

fn persist_invocation(job: &Path, invocation: &RunInvocation) -> Result<()> {
    let path = job.join(INVOCATION_FILE);
    if path.is_file() {
        let retained: RunInvocation = read_json(&path)?;
        if retained != *invocation {
            return Err(eyre!(
                "retry invocation conflicts with durable {}",
                path.display()
            ));
        }
        return Ok(());
    }
    write_json_atomic(&path, invocation)
}

fn record_last_run(job: &Path) -> Result<()> {
    let job = fs::canonicalize(job)?;
    write_json_atomic(Path::new(LAST_RUN_FILE), &LastRun { job })
}

fn read_json<T>(path: &Path) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| eyre!("JSON path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut temporary, value)?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .map(|_| ())
        .map_err(Into::into)
}

fn report_resume(eval: &Nanoeval, skipped: usize, total: usize) {
    if eval.resumed() {
        eprintln!(
            "Resuming Harbor job {} ({skipped} of {total} attempts already durable)",
            eval.directory().display(),
        );
    }
}

fn finish_run(error: Option<nanoeval::EvalError>) -> Result<()> {
    error.map_or(Ok(()), |error| Err(error.into()))
}

fn load_prioritized_tasks(
    task_paths: Vec<PathBuf>,
    output: &Path,
) -> Result<(Vec<Task>, Duration)> {
    let started_at = Instant::now();
    let mut tasks = load_tasks(task_paths, Vec::new())?;
    prioritize_tasks(&mut tasks, output)?;
    Ok((tasks, started_at.elapsed()))
}

async fn prepare_run_vm(vm: bool, rootfs: Option<&Path>) -> Result<(PathBuf, PathBuf, Duration)> {
    let vmm = std::env::current_exe()?;
    let started_at = Instant::now();
    let runtime = prepare_runtime_for_vm(vm, rootfs).await?;
    Ok((vmm, runtime, started_at.elapsed()))
}

struct FinishedEvaluation {
    job: HarborJob,
    outcomes: Vec<AttemptOutcome>,
    results: Vec<EvalResult>,
    run_error: Option<nanoeval::EvalError>,
    failed: usize,
    harbor_finish: Duration,
}

async fn finish_evaluation(
    harbor: HarborRecorder,
    remaining_attempts: usize,
    progress: tokio::task::JoinHandle<Result<Progress>>,
    sweep_result: Result<SweepResults, nanoeval::EvalError>,
) -> Result<FinishedEvaluation> {
    let started_at = Instant::now();
    let job = harbor.finish_all(remaining_attempts).await?;
    let progress = progress.await??;
    let (results, run_error) = match sweep_result {
        Ok(results) => (results.into_results(), None),
        Err(error) => (progress.scored_results(), Some(error)),
    };
    Ok(FinishedEvaluation {
        job,
        outcomes: progress.outcomes,
        results,
        run_error,
        failed: progress.failed,
        harbor_finish: started_at.elapsed(),
    })
}

fn bind_finite_run(evaluator: NanoevalBuilder, sweep: &Sweep, fresh: bool) -> NanoevalBuilder {
    if fresh {
        evaluator.fresh_run(sweep)
    } else {
        evaluator.resume_incomplete(sweep)
    }
}

fn configure_memory_limit(
    evaluator: NanoevalBuilder,
    max_memory_mb: Option<u64>,
) -> NanoevalBuilder {
    match max_memory_mb {
        Some(max_memory_mb) => evaluator.max_memory_mb(max_memory_mb),
        None => evaluator,
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
enum VmRetention {
    /// Retain disks only for failures, refusals, and errors.
    #[default]
    Failures,
    /// Retain disks for every attempt, including passes.
    All,
}

impl VmRetention {
    const fn retains_passes(self) -> bool {
        matches!(self, Self::All)
    }
}

fn load_tasks(paths: Vec<PathBuf>, suites: Vec<PathBuf>) -> Result<Vec<Task>> {
    load_task_paths(paths, suites)?
        .into_iter()
        .map(Task::load)
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn prioritize_tasks(tasks: &mut [Task], output: &Path) -> Result<()> {
    let estimates = retained_task_durations(output)?;
    tasks.sort_by_key(|task| {
        let declared_floor = task
            .agent_timeout()
            .div_f64(4.0)
            .min(Duration::from_secs(600));
        let estimate = estimates
            .get(task.name())
            .copied()
            .unwrap_or(declared_floor);
        Reverse((estimate, task.agent_timeout(), task.verifier().timeout()))
    });
    Ok(())
}

fn retained_task_durations(output: &Path) -> Result<BTreeMap<String, Duration>> {
    let jobs = match fs::read_dir(output) {
        Ok(jobs) => jobs,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error.into()),
    };
    let mut samples = BTreeMap::<String, Vec<Duration>>::new();
    for job in jobs {
        let job = job?;
        if !job.file_type()?.is_dir() {
            continue;
        }
        for trial in fs::read_dir(job.path())? {
            let trial = trial?;
            if !trial.file_type()?.is_dir() {
                continue;
            }
            let Ok(bytes) = fs::read(trial.path().join("result.json")) else {
                continue;
            };
            let Ok(result) = serde_json::from_slice::<RetainedTrialTiming>(&bytes) else {
                continue;
            };
            if result.exception_info.as_ref().is_some_and(|exception| {
                matches!(
                    exception.exception_type.as_str(),
                    "EnvironmentError" | "VerifierError" | "NanoevalError"
                )
            }) {
                continue;
            }
            let Ok(duration) = result
                .finished_at
                .signed_duration_since(result.started_at)
                .to_std()
            else {
                continue;
            };
            samples.entry(result.task_name).or_default().push(duration);
        }
    }
    Ok(samples
        .into_iter()
        .map(|(task, mut durations)| {
            durations.sort_unstable();
            let median = durations[durations.len() / 2];
            (task, median)
        })
        .collect())
}

#[derive(Deserialize)]
struct RetainedTrialTiming {
    task_name: String,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    exception_info: Option<RetainedException>,
}

#[derive(Deserialize)]
struct RetainedException {
    exception_type: String,
}

pub(crate) fn load_task_paths(
    mut paths: Vec<PathBuf>,
    suites: Vec<PathBuf>,
) -> Result<Vec<PathBuf>> {
    for suite in suites {
        let mut suite_tasks = fs::read_dir(&suite)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir() && path.join("task.toml").is_file())
            .collect::<Vec<_>>();
        suite_tasks.sort();
        if suite_tasks.is_empty() {
            return Err(eyre!(
                "suite contains no immediate task directories: {}",
                suite.display()
            ));
        }
        paths.extend(suite_tasks);
    }
    Ok(paths)
}

async fn prepare_network_for_vm(enabled: bool) -> Result<Option<PathBuf>> {
    if enabled {
        Ok(Some(prepare_gvproxy(Path::new(DEFAULT_VM_CACHE)).await?))
    } else {
        Ok(None)
    }
}

async fn selected_vm_environments(
    tasks: &[Task],
    vm: bool,
    rootfs: Option<PathBuf>,
    refresh: bool,
    vmm: &Path,
    runtime_image: &Path,
) -> Result<Option<BTreeMap<PathBuf, VmEnvironment>>> {
    if let Some(rootfs) = rootfs {
        let workspace = if rootfs.is_file() {
            "/app"
        } else {
            "/workspace"
        };
        let environment = VmEnvironment {
            rootfs,
            workspace: workspace.to_owned(),
            environment: BTreeMap::new(),
            shell: "bash".to_owned(),
        };
        return Ok(Some(
            tasks
                .iter()
                .map(|task| (task.root().to_path_buf(), environment.clone()))
                .collect(),
        ));
    }
    if !vm {
        return Ok(None);
    }
    let policy = if refresh {
        CachePolicy::Refresh
    } else {
        CachePolicy::Reuse
    };
    let image_builder =
        VmImageBuilder::new(vmm, runtime_image, Path::new(DEFAULT_KRUNFW_DIRECTORY));
    Ok(Some(
        prepare_vm_environments(tasks, Path::new(DEFAULT_VM_CACHE), policy, &image_builder).await?,
    ))
}

struct RunMeasurements {
    observability: Duration,
    task_loading: Duration,
    vm_runtime: Duration,
    vm_environments: Duration,
    evaluation_setup: Duration,
    attempts: Duration,
    harbor_finish: Duration,
    output: Duration,
    total: Duration,
}

impl RunMeasurements {
    fn record(&self, results: &[EvalResult], attempt_count: usize, errored_attempt_count: usize) {
        let model_ns = results
            .iter()
            .map(|result| result.agent.metadata.model_duration_ns)
            .sum::<u64>();
        let warmup_ns = results
            .iter()
            .map(|result| result.agent.metadata.warmup_duration_ns)
            .sum::<u64>();
        let tool_work_ns = results
            .iter()
            .map(|result| result.agent.metadata.tool_work_duration_ns)
            .sum::<u64>();
        let tool_wall_ns = results
            .iter()
            .map(|result| result.agent.metadata.tool_wall_duration_ns)
            .sum::<u64>();
        let verifier_ns = results
            .iter()
            .map(|result| {
                result
                    .timing
                    .verifier
                    .finished_at
                    .signed_duration_since(result.timing.verifier.started_at)
                    .num_nanoseconds()
                    .and_then(|duration| u64::try_from(duration).ok())
                    .unwrap_or_default()
            })
            .sum::<u64>();
        let response_retries = results
            .iter()
            .map(|result| u64::from(result.agent.metadata.response_retries))
            .sum::<u64>();
        let cached_input_tokens = results
            .iter()
            .map(|result| result.agent.usage.cached_input_tokens)
            .sum::<u64>();
        let input_tokens = results
            .iter()
            .map(|result| result.agent.usage.input_tokens)
            .sum::<u64>();
        info!(
            target: "nanoeval",
            duration_ns = duration_ns(self.total),
            observability_duration_ns = duration_ns(self.observability),
            task_loading_duration_ns = duration_ns(self.task_loading),
            vm_runtime_duration_ns = duration_ns(self.vm_runtime),
            vm_environments_duration_ns = duration_ns(self.vm_environments),
            evaluation_setup_duration_ns = duration_ns(self.evaluation_setup),
            attempts_wall_duration_ns = duration_ns(self.attempts),
            harbor_finish_duration_ns = duration_ns(self.harbor_finish),
            output_duration_ns = duration_ns(self.output),
            attempt_count,
            scored_attempt_count = results.len(),
            errored_attempt_count,
            attempts_model_duration_ns = model_ns,
            attempts_warmup_duration_ns = warmup_ns,
            attempts_tool_work_duration_ns = tool_work_ns,
            attempts_tool_wall_duration_ns = tool_wall_ns,
            attempts_verifier_duration_ns = verifier_ns,
            response_retries,
            input_tokens,
            cached_input_tokens,
            "evaluation run completed"
        );
    }
}

fn duration_ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

async fn prepare_vm_environments(
    tasks: &[Task],
    cache: &Path,
    policy: CachePolicy,
    builder: &VmImageBuilder,
) -> Result<BTreeMap<PathBuf, VmEnvironment>> {
    let mut environments = BTreeMap::new();
    for task in tasks {
        if environments.contains_key(task.root()) {
            continue;
        }
        let prepared = PreparedRootDisk::prepare(task, cache, policy, builder).await?;
        info!(
            target: "nanoeval",
            task_name = task.name(),
            oci_manifest_digest = prepared.manifest_digest(),
            oci_manifest_source = prepared.manifest_source().as_str(),
            vm_rootfs_cache_status = prepared.disk_status().as_str(),
            vm_rootfs_path = %prepared.path().display(),
            "VM root disk ready"
        );
        environments.insert(
            task.root().to_path_buf(),
            VmEnvironment {
                rootfs: prepared.path().to_path_buf(),
                workspace: prepared.workdir().to_owned(),
                environment: prepared.environment().clone(),
                shell: prepared.shell().to_owned(),
            },
        );
    }
    Ok(environments)
}

async fn prepare_runtime_for_vm(vm: bool, rootfs: Option<&Path>) -> Result<PathBuf> {
    if vm || rootfs.is_some_and(Path::is_file) {
        prepare_vm_guest_runtime().await
    } else {
        Ok(PathBuf::new())
    }
}

const EMBEDDED_GUEST_TOOL_RUNTIME: &str = "/usr/local/bin/nanocodex-vm-guest";
const BLOCK_GUEST_TOOL_RUNTIME: &str = "/run/nanoeval/nanocodex-vm-guest";
const GUEST_RUNTIME_BLOCK_ID: &str = "nanoeval-runtime";
const GUEST_RUNTIME_BLOCK_DEVICE: &str = "/dev/vdb";
const GUEST_RUNTIME_MOUNT: &str = "/run/nanoeval";
const DEFAULT_VM_CACHE: &str = ".cache/vm";
const DEFAULT_KRUNFW_DIRECTORY: &str = ".cache/libkrunfw/libkrunfw";
const VM_GUEST_TARGET: &str = "aarch64-unknown-linux-musl";
const VM_GUEST_RUNTIME_RECORD_VERSION: u32 = 2;
// arcbox-ext4 uses a 32,768-block minimum geometry. Keep the backing file at
// least that large so the Linux ext4 driver sees the complete filesystem.
const VM_GUEST_RUNTIME_DISK_BYTES: u64 = 128 * 1024 * 1024;
const VERIFIER_CACHE_VERSION: u32 = 2;
const MINIMUM_VERIFIER_CACHE_DISK_BYTES: u64 = 512 * 1024 * 1024;
const VERIFIER_SETUP_MARKER: &str = "# Check if we're in a valid working directory";
const VERIFIER_CACHE_BLOCK_ID: &str = "nanoeval-verifier-cache";
const VERIFIER_CACHE_BLOCK_DEVICE: &str = "/dev/vdc";
const VERIFIER_CACHE_MOUNT: &str = "/run/nanoeval-verifier-cache";
const CACHED_VERIFIER_SCRIPT: &str = "/tmp/nanoeval-verifier.sh";

#[derive(Clone)]
struct VmEnvironment {
    rootfs: PathBuf,
    workspace: String,
    environment: BTreeMap<String, String>,
    shell: String,
}

#[derive(Clone, Copy)]
struct VmAttemptHost<'a> {
    runtime_image: &'a Path,
    vmm: &'a Path,
    gvproxy: &'a Path,
    retain_passed_rootfs: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct VmGuestRuntimeRecord {
    version: u32,
    target: String,
    binary_size: u64,
    binary_modified_unix_ns: u64,
    digest: String,
}

pub(crate) async fn prepare_vm_guest_runtime() -> Result<PathBuf> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| eyre!("nanoeval binary crate is not inside its Cargo workspace"))?;
    let started_at = Instant::now();
    let runtime = workspace
        .join("target")
        .join(VM_GUEST_TARGET)
        .join("debug/nanocodex-vm-guest");
    let runtime_is_fresh = vm_guest_runtime_is_fresh(workspace, &runtime)?;
    let cached = if runtime_is_fresh {
        load_vm_guest_runtime_record(workspace, &runtime)?
    } else {
        None
    };
    let build_status = if cached.is_some() {
        "hit"
    } else if runtime_is_fresh {
        "indexed"
    } else {
        let exit = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .current_dir(workspace)
            .arg("build")
            .arg("--quiet")
            .arg("--target")
            .arg(VM_GUEST_TARGET)
            .arg("--package")
            .arg("nanocodex-vm")
            .arg("--bin")
            .arg("nanocodex-vm-guest")
            .status()
            .await?;
        if !exit.success() {
            return Err(eyre!("building the VM guest runtime failed with {exit}"));
        }
        "rebuilt"
    };
    if !runtime.is_file() {
        return Err(eyre!(
            "Cargo completed without producing {}",
            runtime.display()
        ));
    }
    let (runtime_digest, runtime_directory) = match cached {
        Some(record) => {
            let directory = vm_guest_runtime_directory(workspace, &record.digest);
            (record.digest, directory)
        }
        None => stage_vm_guest_runtime(workspace, &runtime)?,
    };
    let runtime_image = runtime_directory.join("runtime.ext4");
    info!(
        target: "nanoeval",
        duration_ns = u64::try_from(started_at.elapsed().as_nanos()).unwrap_or(u64::MAX),
        vm_guest_build_status = build_status,
        vm_guest_target = VM_GUEST_TARGET,
        vm_guest_runtime_digest = runtime_digest,
        vm_guest_runtime_disk = %runtime_image.display(),
        "VM guest runtime ready"
    );
    Ok(runtime_image)
}

fn load_vm_guest_runtime_record(
    workspace: &Path,
    runtime: &Path,
) -> Result<Option<VmGuestRuntimeRecord>> {
    let path = vm_guest_runtime_record_path(workspace);
    let contents = match fs::read(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let record = match serde_json::from_slice::<VmGuestRuntimeRecord>(&contents) {
        Ok(record) => record,
        Err(error) => {
            warn!(
                target: "nanoeval",
                cache_record = %path.display(),
                %error,
                "ignoring invalid VM guest runtime cache record"
            );
            return Ok(None);
        }
    };
    let metadata = fs::metadata(runtime)?;
    if record.version != VM_GUEST_RUNTIME_RECORD_VERSION
        || record.target != VM_GUEST_TARGET
        || record.binary_size != metadata.len()
        || record.binary_modified_unix_ns != modified_unix_ns(&metadata)?
        || !is_sha256_digest(&record.digest)
    {
        return Ok(None);
    }
    let runtime_directory = vm_guest_runtime_directory(workspace, &record.digest);
    let staged_runtime = runtime_directory.join("nanocodex-vm-guest");
    let staged_metadata = match fs::metadata(staged_runtime) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !staged_metadata.is_file() || staged_metadata.len() != record.binary_size {
        return Ok(None);
    }
    let runtime_image = runtime_directory.join("runtime.ext4");
    let runtime_image_metadata = match fs::metadata(&runtime_image) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !runtime_image_metadata.is_file()
        || runtime_image_metadata.len() != VM_GUEST_RUNTIME_DISK_BYTES
    {
        return Ok(None);
    }
    let Ok(mut reader) = Reader::new(&runtime_image) else {
        return Ok(None);
    };
    let Ok((_, inode)) = reader.stat("/nanocodex-vm-guest") else {
        return Ok(None);
    };
    if !inode.is_reg() || inode.file_size() != record.binary_size {
        return Ok(None);
    }
    Ok(Some(record))
}

fn stage_vm_guest_runtime(workspace: &Path, runtime: &Path) -> Result<(String, PathBuf)> {
    let runtime_bytes = fs::read(runtime)?;
    let mut runtime_identity = Sha256::new();
    runtime_identity.update(b"nanoeval-vm-guest-runtime-v2\0");
    runtime_identity.update(&runtime_bytes);
    let runtime_digest = format!("{:x}", runtime_identity.finalize());
    let runtime_directory = vm_guest_runtime_directory(workspace, &runtime_digest);
    let staged_runtime = runtime_directory.join("nanocodex-vm-guest");
    if !staged_runtime.is_file() {
        fs::create_dir_all(&runtime_directory)?;
        let temporary = staged_runtime.with_extension(format!("{}.tmp", std::process::id()));
        fs::write(&temporary, &runtime_bytes)?;
        let mut permissions = fs::metadata(&temporary)?.permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        fs::set_permissions(&temporary, permissions)?;
        fs::rename(temporary, &staged_runtime)?;
    }
    let runtime_image = runtime_directory.join("runtime.ext4");
    if !runtime_image.is_file() {
        let temporary = runtime_image.with_extension(format!("{}.tmp", std::process::id()));
        let mut runtime_reader = runtime_bytes.as_slice();
        let mut formatter = Formatter::new(&temporary, 4_096, VM_GUEST_RUNTIME_DISK_BYTES)?;
        formatter.create(
            "/nanocodex-vm-guest",
            make_mode(file_mode::S_IFREG, 0o755),
            None,
            None,
            Some(&mut runtime_reader),
            Some(0),
            Some(0),
            None,
        )?;
        formatter.close()?;
        fs::rename(temporary, &runtime_image)?;
    }
    let metadata = fs::metadata(runtime)?;
    let record = VmGuestRuntimeRecord {
        version: VM_GUEST_RUNTIME_RECORD_VERSION,
        target: VM_GUEST_TARGET.to_owned(),
        binary_size: metadata.len(),
        binary_modified_unix_ns: modified_unix_ns(&metadata)?,
        digest: runtime_digest.clone(),
    };
    let record_path = vm_guest_runtime_record_path(workspace);
    let parent = record_path
        .parent()
        .ok_or_else(|| eyre!("VM guest runtime record has no parent directory"))?;
    fs::create_dir_all(parent)?;
    let temporary = record_path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&temporary, serde_json::to_vec_pretty(&record)?)?;
    fs::rename(temporary, record_path)?;
    Ok((runtime_digest, runtime_directory))
}

fn vm_guest_runtime_directory(workspace: &Path, digest: &str) -> PathBuf {
    workspace
        .join(DEFAULT_VM_CACHE)
        .join("runtimes")
        .join(digest)
}

fn vm_guest_runtime_record_path(workspace: &Path) -> PathBuf {
    workspace
        .join(DEFAULT_VM_CACHE)
        .join("runtime-records")
        .join(format!("{VM_GUEST_TARGET}.json"))
}

fn modified_unix_ns(metadata: &fs::Metadata) -> io::Result<u64> {
    let elapsed = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    u64::try_from(elapsed.as_nanos()).map_err(io::Error::other)
}

fn is_sha256_digest(digest: &str) -> bool {
    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn vm_guest_runtime_is_fresh(workspace: &Path, runtime: &Path) -> io::Result<bool> {
    let built_at = match fs::metadata(runtime) {
        Ok(metadata) => metadata.modified()?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    for input in [
        "Cargo.toml",
        "Cargo.lock",
        ".cargo/config.toml",
        "scripts/aarch64-unknown-linux-musl-linker",
        "scripts/aarch64-unknown-linux-musl-ar",
        "crates/nanocodex-vm/Cargo.toml",
        "crates/nanocodex-vm/src",
    ] {
        if path_changed_after(&workspace.join(input), built_at)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn path_changed_after(path: &Path, built_at: SystemTime) -> io::Result<bool> {
    let metadata = fs::metadata(path)?;
    if metadata.is_file() {
        return Ok(metadata.modified()? > built_at);
    }
    for entry in fs::read_dir(path)? {
        if path_changed_after(&entry?.path(), built_at)? {
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(Debug, thiserror::Error)]
enum VmAttemptError {
    #[error("no VM environment was prepared for task root {0}")]
    MissingPreparedEnvironment(PathBuf),

    #[error("the agent VM session was already finished")]
    AgentSessionAlreadyFinished,

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

    #[error(transparent)]
    ParseReward(#[from] ParseFloatError),

    #[error(transparent)]
    Ext4(#[from] arcbox_ext4::error::FormatError),

    #[error(transparent)]
    Network(#[from] GvproxyError),
}

struct VmAttempt {
    tools: Tools,
    verifier: VmVerifier,
}

struct VmVerifier {
    agent_session: Option<VmToolSession>,
    launch: VmLaunch,
    cache: Option<VerifierCache>,
    attempt_cache: Option<AttemptVerifierCache>,
    retain_passed_rootfs: bool,
    _network: Gvproxy,
}

#[derive(Clone)]
struct VmLaunch {
    root: PathBuf,
    workspace: String,
    shell: String,
    runtime_image: PathBuf,
    vmm: PathBuf,
    cpus: u32,
    memory_mib: u64,
    ext4: bool,
    resolver_configuration: String,
    environment: BTreeMap<String, String>,
    network_socket: PathBuf,
}

struct VerifierCache {
    root: PathBuf,
    key: String,
    status: &'static str,
    cacheable_start: usize,
    cacheable_end: usize,
    disk_bytes: u64,
}

struct AttemptVerifierCache {
    disk: PathBuf,
    skip_setup: bool,
}

fn vm_attempt(
    environment: &VmEnvironment,
    host: VmAttemptHost<'_>,
    attempt: EvalAttempt<'_>,
) -> Result<VmAttempt, VmAttemptError> {
    let span = info_span!(
        target: "nanoeval",
        "vm.attempt.setup",
        otel.kind = "internal",
        otel.status_code = tracing::field::Empty,
        eval.task.name = attempt.task().name(),
        vm.rootfs.template = %environment.rootfs.display(),
        vm.rootfs.destination = %attempt.directory().display(),
        vm.cpu.count = attempt.task().resources().cpus,
        vm.memory_mib = attempt.task().resources().memory_mb,
        status = tracing::field::Empty,
        error.message = tracing::field::Empty,
        duration_ns = tracing::field::Empty,
    );
    let started_at = Instant::now();
    let result = span.in_scope(|| vm_attempt_inner(environment, host, attempt));
    record_operation(&span, started_at, &result);
    result
}

fn vm_attempt_inner(
    environment: &VmEnvironment,
    host: VmAttemptHost<'_>,
    attempt: EvalAttempt<'_>,
) -> Result<VmAttempt, VmAttemptError> {
    let template = &environment.rootfs;
    let verifier_cache = if template.is_file() {
        VerifierCache::prepare(template, attempt.task(), Path::new(DEFAULT_VM_CACHE))?
    } else {
        None
    };
    let root = if template.is_file() {
        if !host.runtime_image.is_file() {
            return Err(VmAttemptError::MissingGuestRuntime(
                host.runtime_image.to_path_buf(),
            ));
        }
        let root = attempt.directory().join("rootfs.ext4");
        reflink_copy::reflink(template, &root)?;
        root
    } else {
        let guest_runtime = template.join(EMBEDDED_GUEST_TOOL_RUNTIME.trim_start_matches('/'));
        if !guest_runtime.is_file() {
            return Err(VmAttemptError::MissingGuestRuntime(guest_runtime));
        }
        {
            let span = info_span!(
                target: "nanoeval",
                "vm.rootfs.materialize",
                otel.kind = "internal",
                otel.status_code = tracing::field::Empty,
                source = %template.display(),
                destination = %attempt.directory().display(),
                status = tracing::field::Empty,
                error.message = tracing::field::Empty,
                duration_ns = tracing::field::Empty,
            );
            let started_at = Instant::now();
            let result = span.in_scope(|| materialize_rootfs(template, attempt.directory()));
            record_operation(&span, started_at, &result);
            result?;
        }
        attempt.directory().to_path_buf()
    };
    let network = Gvproxy::spawn(
        host.gvproxy,
        &attempt.directory().join("vm").join("gvproxy.log"),
    )?;
    let launch = VmLaunch {
        root,
        workspace: environment.workspace.clone(),
        shell: environment.shell.clone(),
        runtime_image: host.runtime_image.to_path_buf(),
        vmm: host.vmm.to_path_buf(),
        cpus: attempt.task().resources().cpus.clamp(1, u32::from(u8::MAX)),
        memory_mib: attempt
            .task()
            .resources()
            .memory_mb
            .clamp(1, u64::from(u32::MAX)),
        ext4: template.is_file(),
        resolver_configuration: "nameserver 192.168.127.1\\n".to_owned(),
        environment: environment.environment.clone(),
        network_socket: network.socket().to_path_buf(),
    };
    let verifier_directory = attempt.directory().join("verifier");
    fs::create_dir_all(&verifier_directory)?;
    let attempt_cache = verifier_cache
        .as_ref()
        .map(|cache| cache.materialize(&verifier_directory))
        .transpose()?;
    let session = launch.spawn(attempt_cache.as_ref())?;
    let vm = VmTools::new(session.clone());
    let tools = Tools::builder()
        .without_defaults()
        .web_search(true)
        .image_generation(true)
        .working_directory(environment.workspace.clone())
        .default_shell(if template.is_file() {
            &environment.shell
        } else {
            "sh"
        })
        .tool(vm.exec_command_tool())
        .tool(vm.write_stdin_tool())
        .tool(vm.apply_patch_tool())
        .tool(vm.view_image_tool())
        .tool(UpdatePlanTool::new())
        .build()
        .map_err(VmAttemptError::from)?;
    Ok(VmAttempt {
        tools,
        verifier: VmVerifier {
            agent_session: Some(session),
            launch,
            cache: verifier_cache,
            attempt_cache,
            retain_passed_rootfs: host.retain_passed_rootfs,
            _network: network,
        },
    })
}

impl VmLaunch {
    fn spawn(
        &self,
        verifier_cache: Option<&AttemptVerifierCache>,
    ) -> Result<VmToolSession, VmAttemptError> {
        let mut command = Command::new(&self.vmm);
        let firmware = Path::new(DEFAULT_KRUNFW_DIRECTORY);
        if firmware.join("libkrunfw.5.dylib").is_file() {
            command.env("DYLD_LIBRARY_PATH", firmware.canonicalize()?);
        }
        command.arg("vm").arg("run").arg("--root").arg(&self.root);
        command.arg("--network-socket").arg(&self.network_socket);
        if self.ext4 {
            command.arg("--ext4").arg("--read-only-disk").arg(format!(
                "{GUEST_RUNTIME_BLOCK_ID}={}",
                self.runtime_image.display()
            ));
            if let Some(cache) = verifier_cache {
                command.arg("--writable-disk").arg(format!(
                    "{VERIFIER_CACHE_BLOCK_ID}={}",
                    cache.disk.display()
                ));
            }
        }
        for (name, value) in &self.environment {
            command.arg("--env").arg(format!("{name}={value}"));
        }
        command
            .arg("--cpus")
            .arg(self.cpus.to_string())
            .arg("--memory-mib")
            .arg(self.memory_mib.to_string())
            .arg(if self.ext4 {
                "/bin/sh"
            } else {
                EMBEDDED_GUEST_TOOL_RUNTIME
            });
        if self.ext4 {
            let resolver_configuration = &self.resolver_configuration;
            command
                .arg("-c")
                .arg(format!(
                    "printf '{resolver_configuration}' > /etc/resolv.conf && mkdir -p \"$1\" /logs/verifier {GUEST_RUNTIME_MOUNT} && mount -t ext4 -o ro {GUEST_RUNTIME_BLOCK_DEVICE} {GUEST_RUNTIME_MOUNT} && exec {BLOCK_GUEST_TOOL_RUNTIME} \"$1\""
                ))
                .arg("nanoeval-init")
                .arg(&self.workspace);
        } else {
            command.arg(&self.workspace);
        }
        VmToolSession::spawn(&mut command).map_err(Into::into)
    }
}

impl VerifierCache {
    fn prepare(template: &Path, task: &Task, cache: &Path) -> Result<Option<Self>, VmAttemptError> {
        let script = fs::read(task.verifier().script())?;
        let Some(setup) = recognized_verifier_setup(&script) else {
            info!(
                target: "nanoeval",
                task_name = task.name(),
                verifier_cache_status = "unsupported",
                "canonical verifier will use the cold dependency path"
            );
            return Ok(None);
        };
        let template_identity = template
            .file_name()
            .ok_or_else(|| io::Error::other("VM root disk template has no file name"))?;
        let mut digest = Sha256::new();
        digest.update(VERIFIER_CACHE_VERSION.to_le_bytes());
        digest.update(VM_GUEST_TARGET.as_bytes());
        digest.update(template_identity.as_encoded_bytes());
        digest.update(&script);
        let disk_bytes = task
            .resources()
            .storage_mb
            .saturating_mul(1024 * 1024)
            .max(MINIMUM_VERIFIER_CACHE_DISK_BYTES);
        digest.update(disk_bytes.to_le_bytes());
        let key = format!("{:x}", digest.finalize());
        let root = cache.join("verifiers").join(&key);
        let disk = root.join("cache.ext4");
        let status = if disk.is_file() && verifier_cache_populated(&disk)? {
            "hit"
        } else {
            "miss"
        };
        info!(
            target: "nanoeval",
            task_name = task.name(),
            verifier_cache_key = key,
            verifier_cache_status = status,
            verifier_cache_path = %root.display(),
            "post-agent verifier dependency cache ready"
        );
        Ok(Some(Self {
            root,
            key,
            status,
            cacheable_start: setup.cacheable_start,
            cacheable_end: setup.cacheable_end,
            disk_bytes,
        }))
    }

    fn materialize(
        &self,
        verifier_directory: &Path,
    ) -> Result<AttemptVerifierCache, VmAttemptError> {
        let disk = verifier_directory.join("cache.ext4");
        if self.status == "hit" {
            reflink_copy::reflink(self.root.join("cache.ext4"), &disk)?;
        } else {
            format_verifier_cache_disk(&disk, self.disk_bytes)?;
        }
        Ok(AttemptVerifierCache {
            disk,
            skip_setup: self.status == "hit",
        })
    }

    fn mark_ready(&self, attempt: &AttemptVerifierCache) -> io::Result<bool> {
        if attempt.skip_setup || !verifier_cache_populated(&attempt.disk)? {
            return Ok(false);
        }
        fs::create_dir_all(&self.root)?;
        let target = self.root.join("cache.ext4");
        let mut identity = Sha256::new();
        identity.update(attempt.disk.as_os_str().as_encoded_bytes());
        let temporary = self
            .root
            .join(format!("cache.{:x}.tmp", identity.finalize()));
        reflink_copy::reflink(&attempt.disk, &temporary)?;
        match fs::hard_link(&temporary, &target) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                fs::remove_file(&temporary)?;
                return Err(error);
            }
        }
        fs::remove_file(temporary)?;
        Ok(true)
    }
}

fn format_verifier_cache_disk(path: &Path, disk_bytes: u64) -> Result<(), VmAttemptError> {
    let mut formatter = Formatter::new(path, 4_096, disk_bytes)?;
    for directory in ["apt-archives", "apt-lists", "uv-cache", "uv-home"] {
        formatter.create(
            &format!("/{directory}"),
            make_mode(file_mode::S_IFDIR, 0o755),
            None,
            None,
            None,
            Some(0),
            Some(0),
            None,
        )?;
    }
    formatter.close()?;
    Ok(())
}

fn verifier_cache_populated(disk: &Path) -> io::Result<bool> {
    let mut reader = Reader::new(disk).map_err(io::Error::other)?;
    Ok(reader.exists("/uv-home/bin/env") && reader.exists("/uv-home/bin/uv"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RecognizedVerifierSetup {
    cacheable_start: usize,
    cacheable_end: usize,
}

fn recognized_verifier_setup(script: &[u8]) -> Option<RecognizedVerifierSetup> {
    let script = std::str::from_utf8(script).ok()?;
    let marker = script.find(VERIFIER_SETUP_MARKER)?;
    let setup = &script[..marker];
    let commands = setup
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect::<Vec<_>>();
    if commands
        != [
            "apt-get update",
            "apt-get install -y curl",
            "curl -LsSf https://astral.sh/uv/0.9.5/install.sh | sh",
            "source $HOME/.local/bin/env",
        ]
    {
        return None;
    }
    let cacheable_start = script
        .strip_prefix("#!")
        .and_then(|script| script.find('\n'))
        .map_or(0, |offset| offset + 3);
    Some(RecognizedVerifierSetup {
        cacheable_start,
        cacheable_end: marker,
    })
}

fn cached_verifier_script(script: &[u8], setup: RecognizedVerifierSetup) -> Vec<u8> {
    let mut cached = Vec::with_capacity(script.len());
    cached.extend_from_slice(&script[..setup.cacheable_start]);
    cached.extend_from_slice(b"\nsource /root/.local/bin/env\n");
    cached.extend_from_slice(&script[setup.cacheable_end..]);
    cached
}

impl AttemptVerifier for VmVerifier {
    fn verify<'a>(
        &'a mut self,
        task: &'a Task,
        attempt: EvalAttempt<'a>,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<AttemptVerification, Box<dyn Error + Send + Sync + 'static>>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.verify_inner(task, attempt)
                .await
                .map_err(|error| Box::new(error) as _)
        })
    }
}

impl VmVerifier {
    async fn verify_inner(
        &mut self,
        task: &Task,
        attempt: EvalAttempt<'_>,
    ) -> Result<AttemptVerification, VmAttemptError> {
        let agent_session = self
            .agent_session
            .take()
            .ok_or(VmAttemptError::AgentSessionAlreadyFinished)?;
        let verifier_directory = attempt.directory().join("verifier");
        fs::create_dir_all(&verifier_directory)?;
        let tests = task
            .verifier()
            .script()
            .parent()
            .ok_or_else(|| io::Error::other("verifier script has no parent directory"))?;
        Self::copy_directory(&agent_session, tests, tests, Path::new("/tests")).await?;
        agent_session
            .write_file("/logs/verifier/.nanoeval", Vec::new(), 0o600)
            .await?;
        if self.attempt_cache.is_some() {
            self.mount_verifier_cache(&agent_session).await?;
        }
        self.stage_cached_verifier(&agent_session, task).await?;
        let command = self.verifier_command(task, self.attempt_cache.as_ref())?;
        let (output, verifier_timed_out) =
            Self::execute_verifier_command(&agent_session, command).await?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let combined = match (stdout.is_empty(), stderr.is_empty()) {
            (_, true) => stdout.clone(),
            (true, false) => stderr.clone(),
            (false, false) => format!("{stdout}\n{stderr}"),
        };
        fs::write(verifier_directory.join("test-stdout.txt"), combined)?;
        let reward_bytes = if verifier_timed_out {
            b"0\n".to_vec()
        } else {
            agent_session.read_file("/logs/verifier/reward.txt").await?
        };
        fs::write(verifier_directory.join("reward.txt"), &reward_bytes)?;
        if let Ok(ctrf) = agent_session.read_file("/logs/verifier/ctrf.json").await {
            fs::write(verifier_directory.join("ctrf.json"), ctrf)?;
        }
        let answer_path = format!("{}/answer.txt", self.launch.workspace);
        if let Ok(answer) = agent_session.read_file(answer_path).await {
            fs::write(attempt.workspace().join("answer.txt"), answer)?;
        }
        agent_session.shutdown().await?;
        if let (Some(cache), Some(attempt_cache)) = (&self.cache, &self.attempt_cache)
            && !attempt_cache.skip_setup
        {
            if cache.mark_ready(attempt_cache)? {
                info!(
                    target: "nanoeval",
                    verifier_cache_key = cache.key,
                    verifier_cache_previous_status = cache.status,
                    "post-agent verifier dependency cache committed"
                );
            } else {
                warn!(
                    target: "nanoeval",
                    verifier_cache_key = cache.key,
                    "verifier dependency cache remained incomplete"
                );
            }
        }
        if let Some(attempt_cache) = self.attempt_cache.take() {
            fs::remove_file(attempt_cache.disk)?;
        }
        let reward = String::from_utf8_lossy(&reward_bytes)
            .trim()
            .parse::<f64>()?;
        if reward > 0.0 && self.launch.ext4 && !self.retain_passed_rootfs {
            match remove_passed_rootfs(&self.launch.root) {
                Ok(true) => info!(
                    target: "nanoeval",
                    vm_rootfs_path = %self.launch.root.display(),
                    "removed passed attempt VM root disk"
                ),
                Ok(false) => {}
                Err(error) => warn!(
                    target: "nanoeval",
                    vm_rootfs_path = %self.launch.root.display(),
                    %error,
                    "failed to remove passed attempt VM root disk"
                ),
            }
        }
        Ok(AttemptVerification {
            result: VerifierResult {
                exit_code: output.exit_code,
                rewards: BTreeMap::from([("reward".to_owned(), reward)]),
            },
            stdout,
            stderr,
        })
    }

    async fn execute_verifier_command(
        session: &VmToolSession,
        command: VmCommand,
    ) -> Result<(VmCommandOutput, bool), VmAttemptError> {
        match session.command(command).await {
            Ok(output) => Ok((output, false)),
            Err(VmToolSessionError::GuestTimeout(timeout)) => Ok((
                VmCommandOutput {
                    exit_code: 124,
                    stdout: Vec::new(),
                    stderr: format!(
                        "canonical verifier exceeded its {timeout:?} deadline; \
                         the candidate is scored with reward 0\n"
                    )
                    .into_bytes(),
                },
                true,
            )),
            Err(error) => Err(error.into()),
        }
    }

    async fn stage_cached_verifier(
        &self,
        session: &VmToolSession,
        task: &Task,
    ) -> Result<(), VmAttemptError> {
        if !self
            .attempt_cache
            .as_ref()
            .is_some_and(|cache| cache.skip_setup)
        {
            return Ok(());
        }
        let cache = self
            .cache
            .as_ref()
            .ok_or_else(|| io::Error::other("verifier cache metadata is missing"))?;
        let script = fs::read(task.verifier().script())?;
        let cached = cached_verifier_script(
            &script,
            RecognizedVerifierSetup {
                cacheable_start: cache.cacheable_start,
                cacheable_end: cache.cacheable_end,
            },
        );
        session
            .write_file(CACHED_VERIFIER_SCRIPT, cached, 0o700)
            .await?;
        Ok(())
    }

    async fn mount_verifier_cache(&self, session: &VmToolSession) -> Result<(), VmAttemptError> {
        let output = session
            .command(
                VmCommand::new("/bin/sh")
                    .arg("-c")
                    .arg(format!(
                        "mkdir -p {VERIFIER_CACHE_MOUNT} /var/cache/apt/archives /var/lib/apt/lists /root/.cache/uv /root/.local && mount -t ext4 {VERIFIER_CACHE_BLOCK_DEVICE} {VERIFIER_CACHE_MOUNT} && mount --bind {VERIFIER_CACHE_MOUNT}/apt-archives /var/cache/apt/archives && mount --bind {VERIFIER_CACHE_MOUNT}/apt-lists /var/lib/apt/lists && mount --bind {VERIFIER_CACHE_MOUNT}/uv-cache /root/.cache/uv && mount --bind {VERIFIER_CACHE_MOUNT}/uv-home /root/.local"
                    ))
                    .timeout(Duration::from_secs(30)),
            )
            .await?;
        if output.exit_code != 0 {
            return Err(io::Error::other(format!(
                "mounting verifier cache exited {}: {}",
                output.exit_code,
                String::from_utf8_lossy(&output.stderr)
            ))
            .into());
        }
        Ok(())
    }

    fn verifier_command(
        &self,
        task: &Task,
        attempt_cache: Option<&AttemptVerifierCache>,
    ) -> Result<VmCommand, VmAttemptError> {
        let skip_setup = attempt_cache.is_some_and(|cache| cache.skip_setup);
        let mut command = if skip_setup {
            let cache = self
                .cache
                .as_ref()
                .ok_or_else(|| io::Error::other("verifier cache metadata is missing"))?;
            info!(
                target: "nanoeval",
                verifier_cache_key = cache.key,
                verifier_setup_bytes_skipped = cache.cacheable_end - cache.cacheable_start,
                verifier_system_setup_bytes = cache.cacheable_start,
                "running canonical verifier with only persisted setup omitted"
            );
            VmCommand::new(verifier_shell(&self.launch.shell, skip_setup))
                .arg(CACHED_VERIFIER_SCRIPT)
        } else {
            VmCommand::new(verifier_shell(&self.launch.shell, skip_setup)).arg("/tests/test.sh")
        };
        command = command
            .current_directory(&self.launch.workspace)
            .environment(base_guest_environment(task, &self.launch.workspace))
            .timeout(task.verifier().timeout());
        Ok(command)
    }

    fn copy_directory<'a>(
        session: &'a VmToolSession,
        root: &'a Path,
        directory: &'a Path,
        destination: &'a Path,
    ) -> Pin<Box<dyn Future<Output = Result<(), VmAttemptError>> + Send + 'a>> {
        Box::pin(async move {
            for entry in fs::read_dir(directory)? {
                let entry = entry?;
                let path = entry.path();
                let relative = path.strip_prefix(root).map_err(io::Error::other)?;
                let guest = destination.join(relative).to_string_lossy().into_owned();
                let file_type = entry.file_type()?;
                if file_type.is_dir() {
                    Self::copy_directory(session, root, &path, destination).await?;
                } else if file_type.is_file() {
                    let mode =
                        std::os::unix::fs::PermissionsExt::mode(&entry.metadata()?.permissions());
                    session.write_file(guest, fs::read(path)?, mode).await?;
                } else {
                    return Err(VmAttemptError::Collision(path));
                }
            }
            Ok(())
        })
    }
}

fn remove_passed_rootfs(rootfs: &Path) -> io::Result<bool> {
    if !rootfs.is_file() {
        return Ok(false);
    }
    fs::remove_file(rootfs)?;
    Ok(true)
}

fn verifier_shell(configured: &str, skip_setup: bool) -> &str {
    if skip_setup { "/bin/bash" } else { configured }
}

fn base_guest_environment(task: &Task, workspace: &str) -> Vec<(String, String)> {
    let mut environment = BTreeMap::from([
        (
            "PATH".to_owned(),
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_owned(),
        ),
        ("HOME".to_owned(), "/root".to_owned()),
        ("NANOEVAL_WORKSPACE".to_owned(), workspace.to_owned()),
        (
            "NANOEVAL_VERIFIER_LOGS".to_owned(),
            "/logs/verifier".to_owned(),
        ),
    ]);
    environment.extend(task.environment().clone());
    environment.extend(task.verifier().environment().clone());
    environment.into_iter().collect()
}

fn record_operation<T, E>(span: &tracing::Span, started_at: Instant, result: &Result<T, E>)
where
    E: std::fmt::Display,
{
    let duration_ns = u64::try_from(started_at.elapsed().as_nanos()).unwrap_or(u64::MAX);
    span.record("duration_ns", duration_ns);
    match result {
        Ok(_) => {
            span.record("status", "completed");
            span.record("otel.status_code", "OK");
            span.in_scope(|| {
                info!(
                    target: "nanoeval",
                    duration_ns,
                    status = "completed",
                    "VM attempt operation completed"
                );
            });
        }
        Err(error) => {
            span.record("status", "failed");
            span.record("otel.status_code", "ERROR");
            span.record("error.message", tracing::field::display(error));
            span.in_scope(|| {
                info!(
                    target: "nanoeval",
                    duration_ns,
                    status = "failed",
                    error = %error,
                    "VM attempt operation failed"
                );
            });
        }
    }
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

#[derive(Serialize)]
struct RunReport {
    job_id: uuid::Uuid,
    job_directory: PathBuf,
    skipped: usize,
    summary: RunSummary,
    attempts: Vec<AttemptOutcome>,
}

impl RunReport {
    fn new(job: &HarborJob, mut attempts: Vec<AttemptOutcome>, skipped: usize) -> Self {
        attempts.sort_by(|left, right| left.trial_name().cmp(right.trial_name()));
        Self {
            job_id: job.id(),
            job_directory: job.directory().to_path_buf(),
            skipped,
            summary: RunSummary::from_attempts(&attempts),
            attempts,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
struct RunSummary {
    total: usize,
    passed: usize,
    failed: usize,
    refused: usize,
    errored: usize,
}

impl RunSummary {
    fn from_attempts(attempts: &[AttemptOutcome]) -> Self {
        let mut summary = Self {
            total: attempts.len(),
            ..Self::default()
        };
        for attempt in attempts {
            match attempt {
                AttemptOutcome::Passed(_) => summary.passed += 1,
                AttemptOutcome::Failed(_) => summary.failed += 1,
                AttemptOutcome::Refused(_) => summary.refused += 1,
                AttemptOutcome::Errored(_) => summary.errored += 1,
            }
        }
        summary
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", content = "details", rename_all = "snake_case")]
enum AttemptOutcome {
    Passed(EvalResult),
    Failed(EvalResult),
    Refused(EvalFailure),
    Errored(EvalFailure),
}

impl AttemptOutcome {
    fn from_result(result: EvalResult) -> Self {
        match result.status {
            EvalStatus::Passed => Self::Passed(result),
            EvalStatus::Failed => Self::Failed(result),
        }
    }

    fn trial_name(&self) -> &str {
        match self {
            Self::Passed(result) | Self::Failed(result) => &result.trial_name,
            Self::Refused(failure) | Self::Errored(failure) => &failure.trial_name,
        }
    }

    fn from_failure(failure: EvalFailure) -> Self {
        if failure.kind == EvalFailureKind::AgentSafetyRefusal {
            Self::Refused(failure)
        } else {
            Self::Errored(failure)
        }
    }
}

struct Progress {
    outcomes: Vec<AttemptOutcome>,
    failed: usize,
}

impl Progress {
    fn scored_results(&self) -> Vec<EvalResult> {
        self.outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                AttemptOutcome::Passed(result) | AttemptOutcome::Failed(result) => {
                    Some(result.clone())
                }
                AttemptOutcome::Refused(_) | AttemptOutcome::Errored(_) => None,
            })
            .collect()
    }
}

async fn report_progress(
    mut events: NanoevalEventStream,
    expected: usize,
    concurrency: usize,
    max_memory_mb: Option<u64>,
) -> Result<Progress> {
    let count = if expected == 1 { "" } else { "s" };
    if let Some(max_memory_mb) = max_memory_mb {
        eprintln!(
            "Running {expected} evaluation{count} (up to {concurrency} concurrent, \
             {max_memory_mb} MiB task-declared memory)"
        );
    } else {
        eprintln!("Running {expected} evaluation{count} (up to {concurrency} concurrent)");
    }
    let mut completed = 0;
    let mut outcomes = Vec::with_capacity(expected);
    let mut failed = 0;
    while completed < expected {
        let event = events
            .recv()
            .await?
            .ok_or_else(|| eyre!("event stream closed after {completed} of {expected} attempts"))?;
        match &event.kind {
            EvalEventKind::Completed(result) => {
                completed += 1;
                let outcome = AttemptOutcome::from_result(result.as_ref().clone());
                write_progress_line(&outcome, completed, expected);
                outcomes.push(outcome);
            }
            EvalEventKind::Failed(failure) => {
                completed += 1;
                failed += 1;
                let outcome = AttemptOutcome::from_failure(failure.as_ref().clone());
                write_progress_line(&outcome, completed, expected);
                outcomes.push(outcome);
            }
            EvalEventKind::AttemptStarted { .. }
            | EvalEventKind::Agent(_)
            | EvalEventKind::VerifierStarted
            | EvalEventKind::VerifierOutput { .. }
            | EvalEventKind::VerifierCompleted(_) => {}
        }
    }
    Ok(Progress { outcomes, failed })
}

fn write_progress_line(outcome: &AttemptOutcome, completed: usize, expected: usize) {
    match outcome {
        AttemptOutcome::Passed(result) => {
            let status = Painted::new(format!("[PASS {completed}/{expected}]")).green();
            eprintln!(
                "{status} {} ({})",
                result.trial_name,
                result_duration(result)
            );
        }
        AttemptOutcome::Failed(result) => {
            let status = Painted::new(format!("[FAIL {completed}/{expected}]")).red();
            eprintln!(
                "{status} {} ({}, reward={:.3})",
                result.trial_name,
                result_duration(result),
                result.verifier.rewards.values().sum::<f64>()
            );
        }
        AttemptOutcome::Refused(failure) => {
            let message = failure.message.lines().next().unwrap_or_default();
            let status = Painted::new(format!("[REFUSED {completed}/{expected}]")).yellow();
            eprintln!(
                "{status} {} ({}): {message}",
                failure.trial_name,
                format_milliseconds(
                    failure
                        .occurred_at
                        .signed_duration_since(failure.started_at)
                        .num_milliseconds()
                )
            );
        }
        AttemptOutcome::Errored(failure) => {
            let message = failure.message.lines().next().unwrap_or_default();
            let status = Painted::new(format!("[ERROR {completed}/{expected}]")).red();
            eprintln!(
                "{status} {} ({:?}, {}): {message}",
                failure.trial_name,
                failure.kind,
                format_milliseconds(
                    failure
                        .occurred_at
                        .signed_duration_since(failure.started_at)
                        .num_milliseconds()
                )
            );
        }
    }
}

fn result_duration(result: &EvalResult) -> String {
    format_milliseconds(
        result
            .timing
            .finished_at
            .signed_duration_since(result.timing.started_at)
            .num_milliseconds(),
    )
}

fn format_milliseconds(milliseconds: i64) -> String {
    let seconds = milliseconds / 1_000;
    let millis = milliseconds.unsigned_abs() % 1_000;
    format!("{seconds}.{millis:03}s")
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use clap::Parser;
    use nanoeval::Task;

    use super::{
        CACHED_VERIFIER_SCRIPT, DEFAULT_HOST_UTILIZATION_PERCENT, Eval, HostResources,
        cached_verifier_script, load_tasks, load_vm_guest_runtime_record,
        recognized_verifier_setup, remove_passed_rootfs, retained_retry_task_names,
        retained_task_durations, stage_vm_guest_runtime, verifier_shell,
    };

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
            "--max-memory-mb",
            "24576",
            "--vm",
        ])
        .unwrap();

        assert_eq!(
            cli.eval.tasks,
            [PathBuf::from("tasks/first"), PathBuf::from("tasks/second")]
        );
        assert_eq!(cli.eval.trials, Some(5));
        assert_eq!(cli.eval.concurrency, Some(10));
        assert_eq!(cli.eval.max_memory_mb, Some(24_576));
        assert_eq!(cli.eval.host_utilization, DEFAULT_HOST_UTILIZATION_PERCENT);
        assert!(cli.eval.vm);
        assert!(!cli.eval.vm_retention.unwrap_or_default().retains_passes());
        assert!(cli.eval.suites.is_empty());
    }

    #[test]
    fn host_defaults_use_the_configured_share_of_cpu_and_memory() {
        let host = HostResources {
            logical_cpus: 10,
            physical_memory_bytes: Some(32 * 1024 * 1024 * 1024),
        };

        let defaults = host.scheduling_defaults(80);

        assert_eq!(defaults.concurrency, 8);
        assert_eq!(defaults.max_memory_mb, Some(26_214));
    }

    #[test]
    fn host_defaults_keep_at_least_one_execution_slot() {
        let host = HostResources {
            logical_cpus: 1,
            physical_memory_bytes: None,
        };

        let defaults = host.scheduling_defaults(1);

        assert_eq!(defaults.concurrency, 1);
        assert_eq!(defaults.max_memory_mb, None);
    }

    #[test]
    fn explicit_scheduler_limits_disable_automatic_resolution() {
        let cli = TestCli::try_parse_from([
            "nanoeval",
            "--task",
            "tasks/first",
            "--concurrency",
            "3",
            "--max-memory-mb",
            "4096",
        ])
        .unwrap();

        let resolved = cli.eval.resolve_run().unwrap();

        assert_eq!(resolved.concurrency, 3);
        assert_eq!(resolved.max_memory_mb, Some(4_096));
        assert_eq!(resolved.automatic_scheduling, None);
    }

    #[test]
    fn passed_vm_retention_is_explicit() {
        let cli = TestCli::try_parse_from([
            "nanoeval",
            "--task",
            "tasks/first",
            "--vm",
            "--vm-retention",
            "all",
        ])
        .unwrap();

        assert!(cli.eval.vm_retention.unwrap().retains_passes());
    }

    #[test]
    fn rerun_is_a_task_source_with_foundry_style_name_filters() {
        let cli = TestCli::try_parse_from([
            "nanoeval",
            "--rerun",
            "webserver",
            "--rerun-from",
            "job-id",
            "--match-task",
            "torch-.*",
            "--match-task",
            "mteb",
            "--include-errored",
            "--list",
        ])
        .unwrap();

        assert!(cli.eval.retry.rerun);
        assert_eq!(cli.eval.retry.rerun_from, Some(PathBuf::from("job-id")));
        assert_eq!(cli.eval.retry.names, ["webserver"]);
        assert_eq!(cli.eval.retry.match_task, ["torch-.*", "mteb"]);
        assert!(cli.eval.retry.statuses.include_errored);
        assert!(cli.eval.retry.list);
        assert!(cli.eval.tasks.is_empty());
        assert!(cli.eval.suites.is_empty());
    }

    #[test]
    fn positional_rerun_names_are_literal_substrings() {
        let cli = TestCli::try_parse_from(["nanoeval", "--rerun", "task.+", "--list"]).unwrap();
        let matcher = super::retry_matcher(&cli.eval.retry).unwrap().unwrap();

        assert!(matcher.is_match("terminal-bench/task.+example"));
        assert!(!matcher.is_match("terminal-bench/taskXYZexample"));
    }

    #[test]
    fn passed_rootfs_cleanup_removes_only_a_disk_file() {
        let directory = tempfile::tempdir().unwrap();
        let rootfs = directory.path().join("rootfs.ext4");
        fs::write(&rootfs, b"guest disk").unwrap();

        assert!(remove_passed_rootfs(&rootfs).unwrap());
        assert!(!rootfs.exists());
        assert!(!remove_passed_rootfs(directory.path()).unwrap());
    }

    #[test]
    fn suite_loads_immediate_tasks_in_name_order() {
        let suite = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tasks");
        let cli = TestCli::try_parse_from([
            "nanoeval",
            "--suite",
            suite.to_str().unwrap(),
            "--concurrency",
            "3",
        ])
        .unwrap();
        let tasks = load_tasks(cli.eval.tasks, cli.eval.suites).unwrap();
        let names = tasks.iter().map(Task::name).collect::<Vec<_>>();

        assert_eq!(
            names,
            [
                "nanoeval/extract-todos",
                "nanoeval/uppercase-message",
                "nanoeval/write-greeting"
            ]
        );
    }

    #[test]
    fn retained_task_duration_uses_the_median_completed_trial() {
        let output = tempfile::tempdir().unwrap();
        let job = output.path().join("job");
        for (trial, finished_at) in [
            ("first", "2026-07-23T00:00:10Z"),
            ("second", "2026-07-23T00:00:30Z"),
            ("third", "2026-07-23T00:00:20Z"),
        ] {
            let directory = job.join(trial);
            fs::create_dir_all(&directory).unwrap();
            fs::write(
                directory.join("result.json"),
                format!(
                    r#"{{"task_name":"terminal-bench/example","started_at":"2026-07-23T00:00:00Z","finished_at":"{finished_at}"}}"#
                ),
            )
            .unwrap();
        }

        let estimates = retained_task_durations(output.path()).unwrap();
        assert_eq!(
            estimates["terminal-bench/example"],
            std::time::Duration::from_secs(20)
        );
    }

    #[test]
    fn retry_selection_distinguishes_scores_refusals_and_errors() {
        let job = tempfile::tempdir().unwrap();
        for (trial, result) in [
            (
                "passed",
                r#"{"task_name":"terminal-bench/passed","verifier_result":{"rewards":{"reward":1.0}},"exception_info":null}"#,
            ),
            (
                "partially-failed",
                r#"{"task_name":"terminal-bench/partially-failed","verifier_result":{"rewards":{"first":1.0,"second":0.0}},"exception_info":null}"#,
            ),
            (
                "failed",
                r#"{"task_name":"terminal-bench/torch-failed","verifier_result":{"rewards":{"reward":0.0}},"exception_info":null}"#,
            ),
            (
                "refused",
                r#"{"task_name":"terminal-bench/refused","verifier_result":null,"exception_info":{"exception_type":"AgentSafetyRefusalError"}}"#,
            ),
            (
                "errored",
                r#"{"task_name":"terminal-bench/errored","verifier_result":null,"exception_info":{"exception_type":"VerifierError"}}"#,
            ),
        ] {
            let directory = job.path().join(trial);
            fs::create_dir(&directory).unwrap();
            fs::write(directory.join("result.json"), result).unwrap();
        }

        let failed = retained_retry_task_names(job.path(), false, false, None).unwrap();
        assert_eq!(
            failed.task_names,
            [
                "terminal-bench/partially-failed".to_owned(),
                "terminal-bench/torch-failed".to_owned()
            ]
            .into()
        );

        let matcher = regex::RegexSet::new(["torch|errored"]).unwrap();
        let selected = retained_retry_task_names(job.path(), true, true, Some(&matcher)).unwrap();
        assert_eq!(
            selected.task_names,
            [
                "terminal-bench/errored".to_owned(),
                "terminal-bench/torch-failed".to_owned()
            ]
            .into()
        );
    }

    #[test]
    fn retry_lineage_overlays_only_tasks_present_in_the_child_job() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("base");
        let child = root.path().join("child");
        for (job, trial, task, reward) in [
            (&base, "first", "terminal-bench/first", 0.0),
            (&base, "second", "terminal-bench/second", 0.0),
            (&child, "first-retry", "terminal-bench/first", 1.0),
        ] {
            let directory = job.join(trial);
            fs::create_dir_all(&directory).unwrap();
            fs::write(
                directory.join("result.json"),
                format!(
                    r#"{{"task_name":"{task}","verifier_result":{{"rewards":{{"reward":{reward}}}}},"exception_info":null}}"#
                ),
            )
            .unwrap();
        }
        super::write_json_atomic(
            &child.join(super::INVOCATION_FILE),
            &super::RunInvocation {
                version: super::INVOCATION_VERSION,
                trials: 1,
                concurrency: 1,
                max_memory_mb: None,
                vm: false,
                vm_rootfs: None,
                vm_retention: super::VmRetention::Failures,
                thinking: "low".to_owned(),
                rerun_from: Some(base.canonicalize().unwrap()),
            },
        )
        .unwrap();

        let queue = retained_retry_task_names(&child, false, false, None).unwrap();

        assert_eq!(queue.lineage.len(), 2);
        assert_eq!(
            queue.task_names,
            ["terminal-bench/second".to_owned()].into()
        );
    }

    #[test]
    fn cold_verifier_uses_the_prepared_environment_shell() {
        assert_eq!(verifier_shell("sh", false), "sh");
        assert_eq!(verifier_shell("bash", false), "bash");
        assert_eq!(verifier_shell("sh", true), "/bin/bash");
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

    #[test]
    fn guest_runtime_record_reuses_only_the_indexed_binary() {
        let workspace = std::env::temp_dir().join(format!(
            "nanoeval-runtime-record-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let runtime = workspace.join("target/aarch64-unknown-linux-musl/debug/guest");
        fs::create_dir_all(runtime.parent().unwrap()).unwrap();
        fs::write(&runtime, b"first-runtime").unwrap();

        let (digest, directory) = stage_vm_guest_runtime(&workspace, &runtime).unwrap();
        let record = load_vm_guest_runtime_record(&workspace, &runtime)
            .unwrap()
            .unwrap();
        assert_eq!(record.digest, digest);
        assert_eq!(
            directory
                .join("nanocodex-vm-guest")
                .metadata()
                .unwrap()
                .len(),
            13
        );

        fs::write(&runtime, b"different-runtime").unwrap();
        assert!(
            load_vm_guest_runtime_record(&workspace, &runtime)
                .unwrap()
                .is_none()
        );
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn cached_verifier_omits_the_complete_pinned_uv_bootstrap() {
        assert!(CACHED_VERIFIER_SCRIPT.starts_with("/tmp/"));
        let supported = br"#!/bin/bash
# Install curl
apt-get update
apt-get install -y curl
# Install uv
curl -LsSf https://astral.sh/uv/0.9.5/install.sh | sh
source $HOME/.local/bin/env
# Check if we're in a valid working directory
uvx pytest
";
        let setup = recognized_verifier_setup(supported).unwrap();
        assert_eq!(&supported[..setup.cacheable_start], b"#!/bin/bash\n");
        let omitted = &supported[setup.cacheable_start..setup.cacheable_end];
        assert!(omitted.windows(7).any(|window| window == b"apt-get"));
        assert!(omitted.windows(9).any(|window| window == b"astral.sh"));
        assert!(omitted.windows(7).any(|window| window == b"source "));
        assert!(!omitted.windows(4).any(|window| window == b"uvx "));
        let transformed = cached_verifier_script(supported, setup);
        let transformed = std::str::from_utf8(&transformed).unwrap();
        assert!(transformed.starts_with("#!/bin/bash\n"));
        assert!(!transformed.contains("apt-get"));
        assert!(transformed.contains("source /root/.local/bin/env"));
        assert!(!transformed.contains("astral.sh"));
        assert!(transformed.contains("uvx pytest"));

        assert!(recognized_verifier_setup(b"pip install pytest\npytest").is_none());
        assert!(
            recognized_verifier_setup(
                br"apt-get update
apt-get install -y curl
curl -LsSf https://astral.sh/uv/latest/install.sh | sh
source $HOME/.local/bin/env
# Check if we're in a valid working directory
"
            )
            .is_none()
        );
        assert!(
            recognized_verifier_setup(
                br"#!/bin/bash
apt-get update
apt-get install -y curl
touch /root/extra-state
curl -LsSf https://astral.sh/uv/0.9.5/install.sh | sh
source $HOME/.local/bin/env
# Check if we're in a valid working directory
"
            )
            .is_none()
        );
    }
}

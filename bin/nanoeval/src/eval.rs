use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    future::Future,
    io,
    net::IpAddr,
    num::ParseFloatError,
    path::{Path, PathBuf},
    pin::Pin,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use arcbox_ext4::{
    Formatter, Reader,
    constants::{file_mode, make_mode},
};
use clap::Args;
use eyre::{Result, eyre};
use nanocodex::{Tools, ToolsBuildError, UpdatePlanTool};
use nanocodex_vm::{VmCommand, VmToolSession, VmToolSessionError, VmTools};
use nanoeval::{
    AttemptAgent, AttemptVerification, AttemptVerifier, EvalAttempt, EvalEventKind, EvalResult,
    Nanoeval, NanoevalEventStream, Sweep, Task, VerifierResult,
};
use nanoeval_harbor::{Harbor, HarborJob};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::process::Command;
use tracing::{info, info_span, warn};

use crate::config::AgentArgs;
use crate::observability::ObservabilityArgs;
use crate::vm_image::{CachePolicy, PreparedRootDisk, VmImageBuilder};

#[derive(Args)]
pub(crate) struct Eval {
    #[command(flatten)]
    observability: ObservabilityArgs,

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

    /// Run workspace tools inside a libkrun microVM.
    #[arg(long)]
    vm: bool,

    /// Override the prepared rootfs directory or raw ext4 image used by `--vm`.
    #[arg(long, value_name = "PATH")]
    vm_rootfs: Option<PathBuf>,

    /// Resolve the task image at the registry instead of reusing its local resolution.
    #[arg(long, requires = "vm", conflicts_with = "vm_rootfs")]
    vm_refresh: bool,

    #[command(flatten)]
    agent: AgentArgs,
}

impl Eval {
    pub(crate) async fn run(self) -> Result<()> {
        let total_started = Instant::now();
        let observability_started = Instant::now();
        let _observability = self.observability.install()?;
        let observability = observability_started.elapsed();
        let task_loading_started = Instant::now();
        let tasks = self
            .tasks
            .into_iter()
            .map(Task::load)
            .collect::<Result<Vec<_>, _>>()?;
        let task_loading = task_loading_started.elapsed();
        let vmm = std::env::current_exe()?;
        let vm_runtime_started = Instant::now();
        let runtime_image = prepare_runtime_for_vm(self.vm, self.vm_rootfs.as_deref()).await?;
        let vm_runtime = vm_runtime_started.elapsed();
        let vm_environments_started = Instant::now();
        let vm_environments = selected_vm_environments(
            &tasks,
            self.vm,
            self.vm_rootfs,
            self.vm_refresh,
            &vmm,
            &runtime_image,
        )
        .await?;
        let vm_environments_duration = vm_environments_started.elapsed();
        let evaluation_setup_started = Instant::now();
        let nanocodex = self.agent.builder()?;
        let sweep = Sweep::builder()
            .tasks(tasks)
            .trials(self.trials)
            .agent("default", nanocodex.clone())?
            .build()?;
        let attempt_count = sweep.attempt_count();
        let mut evaluator = Nanoeval::builder(nanocodex)
            .output_directory(self.output)
            .max_concurrency(usize::from(self.concurrency));
        if let Some(environments) = vm_environments {
            evaluator = evaluator.attempt_agent(move |attempt, builder| {
                let environment = environments.get(attempt.task().root()).ok_or_else(|| {
                    VmAttemptError::MissingPreparedEnvironment(attempt.task().root().to_path_buf())
                })?;
                let runtime = vm_attempt(
                    &environment.rootfs,
                    &environment.workspace,
                    &environment.environment,
                    &environment.shell,
                    &runtime_image,
                    &vmm,
                    attempt,
                )?;
                Ok::<_, VmAttemptError>(
                    AttemptAgent::new(builder.tools(runtime.tools)).verifier(runtime.verifier),
                )
            });
        }
        let (eval, events) = evaluator.build()?;
        let harbor = Harbor::new(&eval)?.record(events.subscribe())?;
        let progress = tokio::spawn(report_progress(events.subscribe(), attempt_count));
        let evaluation_setup = evaluation_setup_started.elapsed();
        let attempts_started = Instant::now();
        let sweep_result = eval.sweep(sweep).await;
        let attempts = attempts_started.elapsed();
        let harbor_finish_started = Instant::now();
        let job = harbor.finish_all(attempt_count).await?;
        let progress = progress.await??;
        let (results, run_error) = match sweep_result {
            Ok(results) => (results.into_results(), None),
            Err(error) => (progress.results, Some(error)),
        };
        let harbor_finish = harbor_finish_started.elapsed();
        let output_started = Instant::now();
        if self.json {
            serde_json::to_writer_pretty(io::stdout().lock(), &results)?;
            println!();
        } else {
            Self::write_summary(&job, &results);
        }
        let output = output_started.elapsed();
        RunMeasurements {
            observability,
            task_loading,
            vm_runtime,
            vm_environments: vm_environments_duration,
            evaluation_setup,
            attempts,
            harbor_finish,
            output,
            total: total_started.elapsed(),
        }
        .record(&results, attempt_count, progress.failed);
        if let Some(error) = run_error {
            return Err(error.into());
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

#[derive(Clone)]
struct VmEnvironment {
    rootfs: PathBuf,
    workspace: String,
    environment: BTreeMap<String, String>,
    shell: String,
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
}

struct VmAttempt {
    tools: Tools,
    verifier: VmVerifier,
}

struct VmVerifier {
    agent_session: Option<VmToolSession>,
    launch: VmLaunch,
    cache: Option<VerifierCache>,
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
}

struct VerifierCache {
    root: PathBuf,
    key: String,
    status: &'static str,
    script_offset: usize,
    disk_bytes: u64,
}

struct AttemptVerifierCache {
    disk: PathBuf,
    skip_setup: bool,
}

fn vm_attempt(
    template: &Path,
    guest_workspace: &str,
    image_environment: &BTreeMap<String, String>,
    default_shell: &str,
    runtime_image: &Path,
    vmm: &Path,
    attempt: EvalAttempt<'_>,
) -> Result<VmAttempt, VmAttemptError> {
    let span = info_span!(
        target: "nanoeval",
        "vm.attempt.setup",
        otel.kind = "internal",
        otel.status_code = tracing::field::Empty,
        eval.task.name = attempt.task().name(),
        vm.rootfs.template = %template.display(),
        vm.rootfs.destination = %attempt.directory().display(),
        vm.cpu.count = attempt.task().resources().cpus,
        vm.memory_mib = attempt.task().resources().memory_mb,
        status = tracing::field::Empty,
        error.message = tracing::field::Empty,
        duration_ns = tracing::field::Empty,
    );
    let started_at = Instant::now();
    let result = span.in_scope(|| {
        vm_attempt_inner(
            template,
            guest_workspace,
            image_environment,
            default_shell,
            runtime_image,
            vmm,
            attempt,
        )
    });
    record_operation(&span, started_at, &result);
    result
}

fn vm_attempt_inner(
    template: &Path,
    guest_workspace: &str,
    image_environment: &BTreeMap<String, String>,
    default_shell: &str,
    runtime_image: &Path,
    vmm: &Path,
    attempt: EvalAttempt<'_>,
) -> Result<VmAttempt, VmAttemptError> {
    let verifier_cache = if template.is_file() {
        VerifierCache::prepare(template, attempt.task(), Path::new(DEFAULT_VM_CACHE))?
    } else {
        None
    };
    let root = if template.is_file() {
        if !runtime_image.is_file() {
            return Err(VmAttemptError::MissingGuestRuntime(
                runtime_image.to_path_buf(),
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
    let launch = VmLaunch {
        root,
        workspace: guest_workspace.to_owned(),
        shell: default_shell.to_owned(),
        runtime_image: runtime_image.to_path_buf(),
        vmm: vmm.to_path_buf(),
        cpus: attempt.task().resources().cpus.clamp(1, u32::from(u8::MAX)),
        memory_mib: attempt
            .task()
            .resources()
            .memory_mb
            .clamp(1, u64::from(u32::MAX)),
        ext4: template.is_file(),
        resolver_configuration: host_resolver_configuration()?,
        environment: image_environment.clone(),
    };
    let session = launch.spawn(None)?;
    let vm = VmTools::new(session.clone());
    let tools = Tools::builder()
        .without_defaults()
        .web_search(true)
        .image_generation(true)
        .working_directory(guest_workspace)
        .default_shell(if template.is_file() {
            default_shell
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
            let cache_mounts = verifier_cache.map_or_else(String::new, |_| {
                format!(
                    " && mkdir -p {VERIFIER_CACHE_MOUNT} /var/cache/apt/archives /var/lib/apt/lists /root/.cache/uv /root/.local && mount -t ext4 {VERIFIER_CACHE_BLOCK_DEVICE} {VERIFIER_CACHE_MOUNT} && mount --bind {VERIFIER_CACHE_MOUNT}/apt-archives /var/cache/apt/archives && mount --bind {VERIFIER_CACHE_MOUNT}/apt-lists /var/lib/apt/lists && mount --bind {VERIFIER_CACHE_MOUNT}/uv-cache /root/.cache/uv && mount --bind {VERIFIER_CACHE_MOUNT}/uv-home /root/.local"
                )
            });
            command
                .arg("-c")
                .arg(format!(
                    "printf '{resolver_configuration}' > /etc/resolv.conf && mkdir -p \"$1\" /logs/verifier {GUEST_RUNTIME_MOUNT} && mount -t ext4 -o ro {GUEST_RUNTIME_BLOCK_DEVICE} {GUEST_RUNTIME_MOUNT}{cache_mounts} && exec {BLOCK_GUEST_TOOL_RUNTIME} \"$1\""
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
            script_offset: setup.len(),
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

fn host_resolver_configuration() -> io::Result<String> {
    let contents = fs::read_to_string("/etc/resolv.conf")?;
    let mut configuration = String::new();
    for line in contents.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() != Some("nameserver") {
            continue;
        }
        let Some(address) = fields.next() else {
            continue;
        };
        if fields.next().is_some() || address.parse::<IpAddr>().is_err() {
            continue;
        }
        configuration.push_str("nameserver ");
        configuration.push_str(address);
        configuration.push_str("\\n");
    }
    if configuration.is_empty() {
        return Err(io::Error::other(
            "host resolver configuration contains no valid nameserver",
        ));
    }
    Ok(configuration)
}

fn recognized_verifier_setup(script: &[u8]) -> Option<&[u8]> {
    let script = std::str::from_utf8(script).ok()?;
    let marker = script.find(VERIFIER_SETUP_MARKER)?;
    let setup = &script[..marker];
    let mut cursor = 0;
    for required in [
        "apt-get update",
        "apt-get install -y curl",
        "https://astral.sh/uv/0.9.5/install.sh",
        "source $HOME/.local/bin/env",
    ] {
        let offset = setup[cursor..].find(required)?;
        cursor += offset + required.len();
    }
    Some(setup.as_bytes())
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
        agent_session.shutdown().await?;
        let verifier_directory = attempt.directory().join("verifier");
        fs::create_dir_all(&verifier_directory)?;
        let verifier_launch = if self.launch.ext4 {
            let verifier_root = verifier_directory.join("rootfs.ext4");
            reflink_copy::reflink(&self.launch.root, &verifier_root)?;
            let mut launch = self.launch.clone();
            launch.root = verifier_root;
            launch
        } else {
            self.launch.clone()
        };
        let attempt_cache = self
            .cache
            .as_ref()
            .map(|cache| cache.materialize(&verifier_directory))
            .transpose()?;
        let session = verifier_launch.spawn(attempt_cache.as_ref())?;
        let tests = task
            .verifier()
            .script()
            .parent()
            .ok_or_else(|| io::Error::other("verifier script has no parent directory"))?;
        Self::copy_directory(&session, tests, tests, Path::new("/tests")).await?;
        session
            .write_file("/logs/verifier/.nanoeval", Vec::new(), 0o600)
            .await?;
        let command = self.verifier_command(task, attempt_cache.as_ref())?;
        let output = session.command(command).await?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let combined = match (stdout.is_empty(), stderr.is_empty()) {
            (_, true) => stdout.clone(),
            (true, false) => stderr.clone(),
            (false, false) => format!("{stdout}\n{stderr}"),
        };
        fs::write(verifier_directory.join("test-stdout.txt"), combined)?;
        let reward_bytes = session.read_file("/logs/verifier/reward.txt").await?;
        fs::write(verifier_directory.join("reward.txt"), &reward_bytes)?;
        if let Ok(ctrf) = session.read_file("/logs/verifier/ctrf.json").await {
            fs::write(verifier_directory.join("ctrf.json"), ctrf)?;
        }
        let answer_path = format!("{}/answer.txt", self.launch.workspace);
        if let Ok(answer) = session.read_file(answer_path).await {
            fs::write(attempt.workspace().join("answer.txt"), answer)?;
        }
        session.shutdown().await?;
        if let (Some(cache), Some(attempt_cache)) = (&self.cache, &attempt_cache)
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
        if let Some(attempt_cache) = attempt_cache {
            fs::remove_file(attempt_cache.disk)?;
        }
        let reward = String::from_utf8_lossy(&reward_bytes)
            .trim()
            .parse::<f64>()?;
        Ok(AttemptVerification {
            result: VerifierResult {
                exit_code: output.exit_code,
                rewards: BTreeMap::from([("reward".to_owned(), reward)]),
            },
            stdout,
            stderr,
        })
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
            let offset = cache.script_offset + 1;
            info!(
                target: "nanoeval",
                verifier_cache_key = cache.key,
                verifier_setup_bytes_skipped = offset - 1,
                "running canonical verifier from its cached post-setup boundary"
            );
            VmCommand::new(verifier_shell(&self.launch.shell, skip_setup))
                .arg("-c")
                .arg("source /root/.local/bin/env && tail -c +\"$1\" /tests/test.sh | /bin/bash")
                .arg("nanoeval-verifier")
                .arg(offset.to_string())
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

struct Progress {
    results: Vec<EvalResult>,
    failed: usize,
}

async fn report_progress(mut events: NanoevalEventStream, expected: usize) -> Result<Progress> {
    let mut completed = 0;
    let mut results = Vec::new();
    let mut failed = 0;
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
                results.push(result.as_ref().clone());
                eprintln!("{}: {:?}", event.trial_name, result.status);
            }
            EvalEventKind::Failed(failure) => {
                completed += 1;
                failed += 1;
                eprintln!("{}: Errored ({:?})", event.trial_name, failure.kind);
            }
            EvalEventKind::Agent(_)
            | EvalEventKind::VerifierStarted
            | EvalEventKind::VerifierOutput { .. }
            | EvalEventKind::VerifierCompleted(_) => {}
        }
    }
    Ok(Progress { results, failed })
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use clap::Parser;

    use super::{
        Eval, load_vm_guest_runtime_record, recognized_verifier_setup, stage_vm_guest_runtime,
        verifier_shell,
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
            "--vm",
        ])
        .unwrap();

        assert_eq!(
            cli.eval.tasks,
            [PathBuf::from("tasks/first"), PathBuf::from("tasks/second")]
        );
        assert_eq!(cli.eval.trials, 5);
        assert_eq!(cli.eval.concurrency, 10);
        assert!(cli.eval.vm);
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
    fn recognizes_only_the_pinned_apt_uv_verifier_setup() {
        let supported = br"#!/bin/bash
apt-get update
apt-get install -y curl
curl -LsSf https://astral.sh/uv/0.9.5/install.sh | sh
source $HOME/.local/bin/env
# Check if we're in a valid working directory
uvx pytest
";
        let setup = recognized_verifier_setup(supported).unwrap();
        assert!(!setup.windows(4).any(|window| window == b"uvx "));

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
    }
}

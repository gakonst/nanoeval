mod checksum;

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ffi::OsString,
    fs::{self, File},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use chrono::{DateTime, Utc};
use nanoeval::{
    AgentMetadata, AtifBuilder, AtifTrajectory, EvalEventKind, EvalEventStreamError, EvalFailure,
    EvalResult, Nanoeval, NanoevalEventStream, PhaseTiming, Task,
};
use serde::Serialize;
use tokio::{sync::oneshot, task::JoinHandle};
use url::Url;
use uuid::Uuid;

use checksum::{directory_hash, package_content_hash};

#[derive(Debug, thiserror::Error)]
pub enum HarborError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error("failed to compile task ignore rules: {0}")]
    Ignore(#[from] ignore::Error),

    #[error("task directory is empty: {0}")]
    EmptyTask(PathBuf),

    #[error("task directory contains a cyclic symbolic link: {0}")]
    CyclicTaskDirectory(PathBuf),

    #[error("trial directory cannot be represented as a file URL: {0}")]
    InvalidTrialPath(PathBuf),

    #[error(transparent)]
    EventStream(#[from] EvalEventStreamError),

    #[error("received events for attempt {0} before attempt.started")]
    MissingAttempt(Uuid),

    #[error("received duplicate attempt.started for attempt {0}")]
    DuplicateAttempt(Uuid),

    #[error("Harbor recorder stopped before finish")]
    RecorderStopped,

    #[error("Nanoeval event stream closed before Harbor recording finished")]
    EventStreamClosed,

    #[error("Harbor recorder task failed: {0}")]
    Join(#[from] tokio::task::JoinError),

    #[error("Harbor recording requires an active Tokio runtime: {0}")]
    Runtime(#[from] tokio::runtime::TryCurrentError),
}

/// Explicit Harbor compatibility adapter for one evaluation job.
pub struct Harbor {
    artifacts: HarborArtifacts,
}

/// Active, streaming Harbor projection of an independent event subscription.
pub struct HarborRecorder {
    finish: Option<oneshot::Sender<FinishRequest>>,
    task: Option<JoinHandle<Result<HarborJob, HarborError>>>,
}

#[derive(Clone, Debug)]
pub struct HarborJob {
    id: Uuid,
    directory: PathBuf,
}

impl Harbor {
    /// Attaches the adapter to a reusable evaluator and its artifact directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the evaluator directory cannot be initialized with
    /// Harbor job metadata.
    pub fn new(eval: &Nanoeval) -> Result<Self, HarborError> {
        Ok(Self {
            artifacts: HarborArtifacts::attach(eval)?,
        })
    }

    /// Starts consuming one independent event subscription immediately.
    ///
    /// # Errors
    ///
    /// Returns an error when called without an active Tokio runtime.
    pub fn record(self, events: NanoevalEventStream) -> Result<HarborRecorder, HarborError> {
        let (finish, finish_receiver) = oneshot::channel();
        let task = tokio::runtime::Handle::try_current()?.spawn(record(
            self.artifacts,
            events,
            finish_receiver,
        ));
        Ok(HarborRecorder {
            finish: Some(finish),
            task: Some(task),
        })
    }
}

impl HarborRecorder {
    /// Waits until every supplied result's terminal event has been recorded,
    /// then commits the final Harbor job result.
    ///
    /// # Errors
    ///
    /// Returns an error on event lag, malformed event payloads, filesystem
    /// failures, or premature recorder termination.
    pub async fn finish(mut self, results: Vec<EvalResult>) -> Result<HarborJob, HarborError> {
        self.finish
            .take()
            .ok_or(HarborError::RecorderStopped)?
            .send(FinishRequest::Results(results))
            .map_err(|_| HarborError::RecorderStopped)?;
        self.task
            .take()
            .ok_or(HarborError::RecorderStopped)?
            .await?
    }

    /// Finishes after the requested number of completed or errored attempts.
    ///
    /// This is the batch boundary used when individual attempts may fail while
    /// the evaluator continues running unrelated work.
    ///
    /// # Errors
    ///
    /// Returns an error on event lag, malformed event payloads, filesystem
    /// failures, premature recorder termination, or a mismatched attempt count.
    pub async fn finish_all(mut self, attempts: usize) -> Result<HarborJob, HarborError> {
        self.finish
            .take()
            .ok_or(HarborError::RecorderStopped)?
            .send(FinishRequest::TerminalCount(attempts))
            .map_err(|_| HarborError::RecorderStopped)?;
        self.task
            .take()
            .ok_or(HarborError::RecorderStopped)?
            .await?
    }
}

impl Drop for HarborRecorder {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl HarborJob {
    #[must_use]
    pub const fn id(&self) -> Uuid {
        self.id
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }
}

struct AttemptRecording {
    events: BufWriter<File>,
    atif: AtifBuilder,
}

enum FinishRequest {
    Results(Vec<EvalResult>),
    TerminalCount(usize),
}

fn finished_attempt_count(
    request: Option<&FinishRequest>,
    completed: &HashSet<Uuid>,
) -> Option<usize> {
    match request? {
        FinishRequest::Results(results)
            if results
                .iter()
                .all(|result| completed.contains(&result.attempt_id)) =>
        {
            Some(results.len())
        }
        FinishRequest::TerminalCount(expected) if completed.len() == *expected => Some(*expected),
        FinishRequest::Results(_) | FinishRequest::TerminalCount(_) => None,
    }
}

async fn record(
    artifacts: HarborArtifacts,
    mut events: NanoevalEventStream,
    mut finish: oneshot::Receiver<FinishRequest>,
) -> Result<HarborJob, HarborError> {
    let mut attempts = HashMap::<Uuid, AttemptRecording>::new();
    let mut completed = HashSet::<Uuid>::new();
    let mut recorded_results = Vec::<EvalResult>::new();
    let mut recorded_failures = Vec::<EvalFailure>::new();
    let mut finish_request = None::<FinishRequest>;

    loop {
        if let Some(n_total_trials) = finished_attempt_count(finish_request.as_ref(), &completed) {
            artifacts.write_job(&recorded_results, &recorded_failures, n_total_trials)?;
            return Ok(HarborJob {
                id: artifacts.job_id,
                directory: artifacts.root.clone(),
            });
        }

        tokio::select! {
            requested = &mut finish, if finish_request.is_none() => {
                finish_request = Some(requested.map_err(|_| HarborError::RecorderStopped)?);
            }
            event = events.recv() => {
                let event = event?.ok_or(HarborError::EventStreamClosed)?;
                match &event.kind {
                    EvalEventKind::AttemptStarted { prompt, .. } => {
                        let writer = artifacts.write_input(
                            event.attempt_id,
                            &event.trial_name,
                            prompt,
                        )?;
                        if attempts.insert(event.attempt_id, AttemptRecording {
                            events: writer,
                            atif: AtifBuilder::default(),
                        }).is_some() {
                            return Err(HarborError::DuplicateAttempt(event.attempt_id));
                        }
                    }
                    EvalEventKind::Agent(agent_event) => {
                        let attempt = attempts
                            .get_mut(&event.attempt_id)
                            .ok_or(HarborError::MissingAttempt(event.attempt_id))?;
                        serde_json::to_writer(&mut attempt.events, agent_event)?;
                        attempt.events.write_all(b"\n")?;
                        attempt.events.flush()?;
                        attempt.atif.apply(agent_event)?;
                    }
                    EvalEventKind::Completed(result) => {
                        let mut attempt = attempts
                            .remove(&event.attempt_id)
                            .ok_or(HarborError::MissingAttempt(event.attempt_id))?;
                        attempt.events.flush()?;
                        let result = result.as_ref().clone();
                        let trajectory = attempt.atif.finish(result.task(), &result.agent);
                        artifacts.write_trial(&result, &trajectory)?;
                        completed.insert(result.attempt_id);
                        recorded_results.push(result);
                        artifacts.write_job(
                            &recorded_results,
                            &recorded_failures,
                            recorded_results.len() + recorded_failures.len(),
                        )?;
                    }
                    EvalEventKind::Failed(failure) => {
                        let trajectory = if let Some(mut attempt) = attempts.remove(&event.attempt_id) {
                            attempt.events.flush()?;
                            attempt.atif.finish_failure(failure.task())
                        } else {
                            let mut events = artifacts.write_input(
                                event.attempt_id,
                                &event.trial_name,
                                failure.task().prompt(),
                            )?;
                            events.flush()?;
                            AtifBuilder::default().finish_failure(failure.task())
                        };
                        let failure = failure.as_ref().clone();
                        artifacts.write_failure(&failure, &trajectory)?;
                        completed.insert(failure.attempt_id);
                        recorded_failures.push(failure);
                        artifacts.write_job(
                            &recorded_results,
                            &recorded_failures,
                            recorded_results.len() + recorded_failures.len(),
                        )?;
                    }
                    EvalEventKind::VerifierStarted
                    | EvalEventKind::VerifierOutput { .. }
                    | EvalEventKind::VerifierCompleted(_) => {}
                }
            }
        }
    }
}

struct HarborArtifacts {
    job_id: Uuid,
    started_at: DateTime<Utc>,
    root: PathBuf,
    jobs_dir: PathBuf,
    max_concurrency: usize,
    recorded_trials: Mutex<Vec<HarborRecordedTrial>>,
}

impl HarborArtifacts {
    fn attach(eval: &Nanoeval) -> Result<Self, HarborError> {
        let artifacts = Self {
            job_id: eval.id(),
            started_at: eval.started_at(),
            root: eval.directory().to_path_buf(),
            jobs_dir: eval.parent_directory().to_path_buf(),
            max_concurrency: eval.max_concurrency(),
            recorded_trials: Mutex::new(Vec::new()),
        };
        Self::write_file(&artifacts.root.join("job.log"), [])?;
        artifacts.write_job_metadata()?;
        artifacts.write_job(&[], &[], 0)?;
        Ok(artifacts)
    }

    fn write_input(
        &self,
        attempt_id: Uuid,
        trial_name: &str,
        prompt: &str,
    ) -> Result<BufWriter<File>, HarborError> {
        let root = self.root.join(trial_name);
        let agent = root.join("agent");
        fs::create_dir_all(&agent)?;
        let input = HarborInput {
            protocol_version: 1,
            request_id: Some(attempt_id.to_string()),
            kind: "input",
            payload: HarborInputPayload {
                instruction: prompt,
            },
        };
        let mut bytes = serde_json::to_vec(&input)?;
        bytes.push(b'\n');
        Self::write_file(&agent.join("input.jsonl"), bytes)?;
        Ok(BufWriter::new(File::create(agent.join("events.jsonl"))?))
    }

    fn write_trial(
        &self,
        result: &EvalResult,
        trajectory: &AtifTrajectory,
    ) -> Result<(), HarborError> {
        let task = result.task();
        let root = &result.artifacts.directory;
        let agent = root.join("agent");
        let task_path = task.root().to_path_buf();
        let task_checksum = directory_hash(task.root())?;
        let task_digest = package_content_hash(task.root())?;
        let config = HarborTrialConfig {
            task: HarborTaskConfig {
                path: task_path.clone(),
                source: Some("nanoeval/local"),
            },
            trial_name: &result.trial_name,
            trials_dir: &self.root,
            agent: harbor_agent_config(&result.agent.model, &result.agent.effort),
            environment: HarborEnvironmentConfig::native(),
            verifier: HarborVerifierConfig::native(),
            artifacts: Vec::new(),
            extra_instruction_paths: Vec::new(),
            job_id: self.job_id,
        };
        Self::write_json(&root.join("config.json"), &config)?;
        Self::write_json(&agent.join("trajectory.json"), trajectory)?;
        Self::write_json(
            &root.join("artifacts/manifest.json"),
            &Vec::<HarborArtifactManifestEntry>::new(),
        )?;

        let trial_uri = Url::from_directory_path(root)
            .map_err(|()| HarborError::InvalidTrialPath(root.clone()))?
            .to_string();
        let trial_result = HarborTrialResult {
            id: result.attempt_id,
            task_name: &result.task_name,
            trial_name: &result.trial_name,
            trial_uri,
            task_id: HarborTaskId { path: task_path },
            source: "nanoeval/local",
            task_checksum,
            config,
            agent_info: HarborAgentInfo {
                name: "nanocodex",
                version: env!("CARGO_PKG_VERSION"),
                model_info: HarborModelInfo {
                    name: &result.agent.model,
                    provider: "openai",
                },
            },
            agent_result: Some(HarborAgentResult {
                n_input_tokens: result.agent.usage.input_tokens,
                n_cache_tokens: result.agent.usage.cached_input_tokens,
                n_output_tokens: result.agent.usage.output_tokens,
                cost_usd: result.agent.cost_usd,
                rollout_details: None,
                metadata: &result.agent.metadata,
            }),
            verifier_result: Some(HarborVerifierResult {
                rewards: &result.verifier.rewards,
            }),
            started_at: result.timing.started_at,
            finished_at: result.timing.finished_at,
            environment_setup: Some(&result.timing.environment_setup),
            agent_setup: Some(&result.timing.agent_setup),
            agent_execution: Some(&result.timing.agent_execution),
            verifier: Some(&result.timing.verifier),
            exception_info: None,
            step_results: None,
        };
        Self::write_json(&root.join("result.json"), &trial_result)?;
        Self::write_file(&root.join("trial.log"), [])?;
        Self::write_file(&agent.join("stderr.log"), [])?;

        let lock = HarborTrialLock::new(
            task,
            &result.agent.model,
            &result.agent.effort,
            &task_digest,
        );
        Self::write_json(&root.join("lock.json"), &lock)?;
        {
            let mut recorded = self
                .recorded_trials
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            recorded.push(HarborRecordedTrial {
                task: HarborTaskConfig {
                    path: task.root().to_path_buf(),
                    source: Some("nanoeval/local"),
                },
                agent: harbor_agent_config(&result.agent.model, &result.agent.effort),
                lock,
            });
        }
        self.write_job_metadata()
    }

    fn write_failure(
        &self,
        failure: &EvalFailure,
        trajectory: &AtifTrajectory,
    ) -> Result<(), HarborError> {
        let task = failure.task();
        let root = &failure.artifacts.directory;
        let agent = root.join("agent");
        let task_path = task.root().to_path_buf();
        let task_checksum = directory_hash(task.root())?;
        let task_digest = package_content_hash(task.root())?;
        let model = trajectory.agent.model_name.as_str();
        let effort = trajectory
            .steps
            .iter()
            .find_map(|step| step.reasoning_effort.as_deref())
            .unwrap_or(&failure.effort);
        let config = HarborTrialConfig {
            task: HarborTaskConfig {
                path: task_path.clone(),
                source: Some("nanoeval/local"),
            },
            trial_name: &failure.trial_name,
            trials_dir: &self.root,
            agent: harbor_agent_config(model, effort),
            environment: HarborEnvironmentConfig::native(),
            verifier: HarborVerifierConfig::native(),
            artifacts: Vec::new(),
            extra_instruction_paths: Vec::new(),
            job_id: self.job_id,
        };
        Self::write_json(&root.join("config.json"), &config)?;
        Self::write_json(&agent.join("trajectory.json"), trajectory)?;
        Self::write_json(
            &root.join("artifacts/manifest.json"),
            &Vec::<HarborArtifactManifestEntry>::new(),
        )?;

        let trial_uri = Url::from_directory_path(root)
            .map_err(|()| HarborError::InvalidTrialPath(root.clone()))?
            .to_string();
        let trial_result = HarborTrialResult {
            id: failure.attempt_id,
            task_name: &failure.task_name,
            trial_name: &failure.trial_name,
            trial_uri,
            task_id: HarborTaskId { path: task_path },
            source: "nanoeval/local",
            task_checksum,
            config,
            agent_info: HarborAgentInfo {
                name: "nanocodex",
                version: env!("CARGO_PKG_VERSION"),
                model_info: HarborModelInfo {
                    name: model,
                    provider: "openai",
                },
            },
            agent_result: None,
            verifier_result: None,
            started_at: failure.started_at,
            finished_at: failure.occurred_at,
            environment_setup: None,
            agent_setup: None,
            agent_execution: None,
            verifier: None,
            exception_info: Some(HarborExceptionInfo {
                exception_type: failure.kind.harbor_exception_type(),
                exception_message: &failure.message,
                exception_traceback: &failure.traceback,
                occurred_at: failure.occurred_at,
            }),
            step_results: None,
        };
        Self::write_json(&root.join("result.json"), &trial_result)?;
        Self::write_file(&root.join("trial.log"), failure.traceback.as_bytes())?;
        Self::write_file(&agent.join("stderr.log"), failure.traceback.as_bytes())?;

        let lock = HarborTrialLock::new(task, model, effort, &task_digest);
        Self::write_json(&root.join("lock.json"), &lock)?;
        {
            let mut recorded = self
                .recorded_trials
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            recorded.push(HarborRecordedTrial {
                task: HarborTaskConfig {
                    path: task.root().to_path_buf(),
                    source: Some("nanoeval/local"),
                },
                agent: harbor_agent_config(model, effort),
                lock,
            });
        }
        self.write_job_metadata()
    }

    fn write_job(
        &self,
        results: &[EvalResult],
        failures: &[EvalFailure],
        n_total_trials: usize,
    ) -> Result<(), HarborError> {
        let now = Utc::now();
        let input_tokens = results
            .iter()
            .map(|result| result.agent.usage.input_tokens)
            .sum();
        let cached_tokens = results
            .iter()
            .map(|result| result.agent.usage.cached_input_tokens)
            .sum();
        let output_tokens = results
            .iter()
            .map(|result| result.agent.usage.output_tokens)
            .sum();
        let reported_costs: Vec<f64> = results
            .iter()
            .filter_map(|result| result.agent.cost_usd)
            .collect();
        let mut reward_stats = BTreeMap::<String, BTreeMap<String, Vec<String>>>::new();
        for result in results {
            for (name, reward) in &result.verifier.rewards {
                reward_stats
                    .entry(name.clone())
                    .or_default()
                    .entry(harbor_float_key(*reward))
                    .or_default()
                    .push(result.trial_name.clone());
            }
        }
        let eval_key = results.first().map_or_else(
            || {
                failures.first().map_or_else(
                    || "nanocodex__nanoeval/local".to_owned(),
                    |failure| format!("nanocodex__{}__nanoeval/local", failure.model),
                )
            },
            |result| format!("nanocodex__{}__nanoeval/local", result.agent.model),
        );
        let mut exception_stats = BTreeMap::<String, Vec<String>>::new();
        for failure in failures {
            exception_stats
                .entry(failure.kind.harbor_exception_type().to_owned())
                .or_default()
                .push(failure.trial_name.clone());
        }
        let eval_stats = HarborAgentDatasetStats {
            n_trials: results.len(),
            n_errors: failures.len(),
            metrics: Vec::new(),
            pass_at_k: BTreeMap::new(),
            reward_stats,
            exception_stats,
        };
        let job = HarborJobResult {
            id: self.job_id,
            started_at: self.started_at,
            updated_at: now,
            finished_at: (!(results.is_empty() && failures.is_empty())).then_some(now),
            n_total_trials,
            stats: HarborJobStats {
                n_completed_trials: results.len() + failures.len(),
                n_errored_trials: failures.len(),
                n_running_trials: 0,
                n_pending_trials: n_total_trials.saturating_sub(results.len() + failures.len()),
                n_cancelled_trials: 0,
                n_retries: 0,
                evals: BTreeMap::from([(eval_key, eval_stats)]),
                n_input_tokens: input_tokens,
                n_cache_tokens: cached_tokens,
                n_output_tokens: output_tokens,
                cost_usd: (!reported_costs.is_empty()).then(|| reported_costs.into_iter().sum()),
            },
        };
        Self::write_json(&self.root.join("result.json"), &job)
    }

    fn write_job_metadata(&self) -> Result<(), HarborError> {
        let recorded = self
            .recorded_trials
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut tasks = Vec::new();
        let mut agents = Vec::new();
        for trial in recorded.iter() {
            if !tasks
                .iter()
                .any(|task: &HarborTaskConfig| task.path == trial.task.path)
            {
                tasks.push(trial.task.clone());
            }
            if !agents.iter().any(|agent: &HarborAgentConfig| {
                agent.name == trial.agent.name && agent.model_name == trial.agent.model_name
            }) {
                agents.push(trial.agent.clone());
            }
        }
        Self::write_json(
            &self.root.join("config.json"),
            &HarborJobConfig {
                job_name: self.job_id.to_string(),
                jobs_dir: self.jobs_dir.clone(),
                n_concurrent_trials: self.max_concurrency,
                quiet: true,
                environment: HarborEnvironmentConfig::native(),
                verifier: HarborVerifierConfig::native(),
                agents,
                tasks,
            },
        )?;
        Self::write_json(
            &self.root.join("lock.json"),
            &HarborJobLock {
                schema_version: 2,
                created_at: self.started_at,
                harbor: HarborLockInfo {},
                n_concurrent_trials: self.max_concurrency,
                retry: HarborRetryConfig::default(),
                trials: recorded.iter().map(|trial| trial.lock.clone()).collect(),
            },
        )
    }

    fn write_json(path: &Path, value: &impl Serialize) -> Result<(), HarborError> {
        let mut bytes = serde_json::to_vec_pretty(value)?;
        bytes.push(b'\n');
        Self::atomic_write(path, bytes)
    }

    fn atomic_write(path: &Path, bytes: impl AsRef<[u8]>) -> Result<(), HarborError> {
        let mut name: OsString = path
            .file_name()
            .map_or_else(|| OsString::from("artifact"), OsString::from);
        name.push(format!(".{}.tmp", Uuid::new_v4()));
        let temporary = path.with_file_name(name);
        Self::write_file(&temporary, bytes)?;
        fs::rename(&temporary, path)?;
        Ok(())
    }

    fn write_file(path: &Path, bytes: impl AsRef<[u8]>) -> Result<(), HarborError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, bytes)?;
        Ok(())
    }
}

fn harbor_agent_config(model: &str, effort: &str) -> HarborAgentConfig {
    HarborAgentConfig {
        name: "nanocodex",
        model_name: format!("openai/{model}"),
        kwargs: HarborAgentKwargs {
            effort: effort.to_owned(),
        },
    }
}

fn harbor_float_key(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.1}")
    } else {
        value.to_string()
    }
}

#[derive(Serialize)]
struct HarborInput<'a> {
    protocol_version: u32,
    request_id: Option<String>,
    #[serde(rename = "type")]
    kind: &'static str,
    payload: HarborInputPayload<'a>,
}

#[derive(Serialize)]
struct HarborInputPayload<'a> {
    instruction: &'a str,
}

struct HarborRecordedTrial {
    task: HarborTaskConfig,
    agent: HarborAgentConfig,
    lock: HarborTrialLock,
}

#[derive(Serialize)]
struct HarborJobConfig {
    job_name: String,
    jobs_dir: PathBuf,
    n_concurrent_trials: usize,
    quiet: bool,
    environment: HarborEnvironmentConfig,
    verifier: HarborVerifierConfig,
    agents: Vec<HarborAgentConfig>,
    tasks: Vec<HarborTaskConfig>,
}

#[derive(Serialize)]
struct HarborJobLock {
    schema_version: u32,
    created_at: DateTime<Utc>,
    harbor: HarborLockInfo,
    n_concurrent_trials: usize,
    retry: HarborRetryConfig,
    trials: Vec<HarborTrialLock>,
}

#[derive(Serialize)]
struct HarborLockInfo {}

#[derive(Serialize)]
struct HarborRetryConfig {
    max_retries: u32,
    exclude_exceptions: Vec<&'static str>,
    wait_multiplier: f64,
    min_wait_sec: f64,
    max_wait_sec: f64,
}

impl Default for HarborRetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 0,
            exclude_exceptions: vec![
                "AgentTimeoutError",
                "VerifierTimeoutError",
                "RewardFileNotFoundError",
                "RewardFileEmptyError",
                "VerifierOutputParseError",
                "ApiUsageLimitError",
                "AgentSafetyRefusalError",
                "AgentAuthenticationError",
                "ModelNotFoundError",
            ],
            wait_multiplier: 1.0,
            min_wait_sec: 1.0,
            max_wait_sec: 60.0,
        }
    }
}

#[derive(Clone, Serialize)]
struct HarborTrialLock {
    schema_version: u32,
    task: HarborTaskLock,
    install_only: bool,
    timeout_multiplier: f64,
    agent: HarborAgentConfig,
    skills: Vec<HarborAgentSkillLock>,
    environment: HarborEnvironmentConfig,
    verifier: HarborVerifierConfig,
}

impl HarborTrialLock {
    fn new(task: &Task, model: &str, effort: &str, digest: &str) -> Self {
        Self {
            schema_version: 1,
            task: HarborTaskLock {
                name: task
                    .name()
                    .rsplit('/')
                    .next()
                    .unwrap_or(task.name())
                    .to_owned(),
                kind: HarborTaskLockKind::Local,
                digest: format!("sha256:{digest}"),
                source: Some("nanoeval/local"),
                path: task.root().to_path_buf(),
            },
            install_only: false,
            timeout_multiplier: 1.0,
            agent: HarborAgentConfig {
                name: "nanocodex",
                model_name: format!("openai/{model}"),
                kwargs: HarborAgentKwargs {
                    effort: effort.to_owned(),
                },
            },
            skills: Vec::new(),
            environment: HarborEnvironmentConfig::native(),
            verifier: HarborVerifierConfig::native(),
        }
    }
}

#[derive(Clone, Serialize)]
struct HarborTaskLock {
    name: String,
    #[serde(rename = "type")]
    kind: HarborTaskLockKind,
    digest: String,
    source: Option<&'static str>,
    path: PathBuf,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "lowercase")]
enum HarborTaskLockKind {
    Local,
}

#[derive(Clone, Serialize)]
struct HarborAgentSkillLock {}

#[derive(Serialize)]
struct HarborTrialConfig<'a> {
    task: HarborTaskConfig,
    trial_name: &'a str,
    trials_dir: &'a Path,
    agent: HarborAgentConfig,
    environment: HarborEnvironmentConfig,
    verifier: HarborVerifierConfig,
    artifacts: Vec<String>,
    extra_instruction_paths: Vec<PathBuf>,
    job_id: Uuid,
}

#[derive(Clone, Serialize)]
struct HarborTaskConfig {
    path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<&'static str>,
}

#[derive(Clone, Serialize)]
struct HarborAgentConfig {
    name: &'static str,
    model_name: String,
    kwargs: HarborAgentKwargs,
}

#[derive(Clone, Serialize)]
struct HarborAgentKwargs {
    effort: String,
}

#[derive(Clone, Serialize)]
struct HarborEnvironmentConfig {
    #[serde(rename = "type")]
    environment_type: Option<HarborEnvironmentType>,
    import_path: &'static str,
    delete: bool,
    cpu_enforcement_policy: ResourceMode,
    memory_enforcement_policy: ResourceMode,
    kwargs: NativeEnvironmentKwargs,
}

impl HarborEnvironmentConfig {
    const fn native() -> Self {
        Self {
            environment_type: None,
            import_path: "nanoeval.native:NativeEnvironment",
            delete: false,
            cpu_enforcement_policy: ResourceMode::Ignore,
            memory_enforcement_policy: ResourceMode::Ignore,
            kwargs: NativeEnvironmentKwargs { backend: "native" },
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "snake_case")]
enum HarborEnvironmentType {}

#[derive(Clone, Serialize)]
#[serde(rename_all = "lowercase")]
enum ResourceMode {
    Ignore,
}

#[derive(Clone, Serialize)]
struct NativeEnvironmentKwargs {
    backend: &'static str,
}

#[derive(Clone, Serialize)]
struct HarborVerifierConfig {
    import_path: &'static str,
}

impl HarborVerifierConfig {
    const fn native() -> Self {
        Self {
            import_path: "nanoeval.native:Verifier",
        }
    }
}

#[derive(Serialize)]
struct HarborTrialResult<'a> {
    id: Uuid,
    task_name: &'a str,
    trial_name: &'a str,
    trial_uri: String,
    task_id: HarborTaskId,
    source: &'static str,
    task_checksum: String,
    config: HarborTrialConfig<'a>,
    agent_info: HarborAgentInfo<'a>,
    agent_result: Option<HarborAgentResult<'a>>,
    verifier_result: Option<HarborVerifierResult<'a>>,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    environment_setup: Option<&'a PhaseTiming>,
    agent_setup: Option<&'a PhaseTiming>,
    agent_execution: Option<&'a PhaseTiming>,
    verifier: Option<&'a PhaseTiming>,
    exception_info: Option<HarborExceptionInfo<'a>>,
    step_results: Option<Vec<HarborStepResult>>,
}

#[derive(Serialize)]
struct HarborExceptionInfo<'a> {
    exception_type: &'a str,
    exception_message: &'a str,
    exception_traceback: &'a str,
    occurred_at: DateTime<Utc>,
}

#[derive(Serialize)]
struct HarborStepResult {}

#[derive(Serialize)]
struct HarborTaskId {
    path: PathBuf,
}

#[derive(Serialize)]
struct HarborAgentInfo<'a> {
    name: &'static str,
    version: &'static str,
    model_info: HarborModelInfo<'a>,
}

#[derive(Serialize)]
struct HarborModelInfo<'a> {
    name: &'a str,
    provider: &'static str,
}

#[derive(Serialize)]
struct HarborAgentResult<'a> {
    n_input_tokens: u64,
    n_cache_tokens: u64,
    n_output_tokens: u64,
    cost_usd: Option<f64>,
    rollout_details: Option<Vec<HarborRolloutDetail>>,
    metadata: &'a AgentMetadata,
}

#[derive(Serialize)]
struct HarborRolloutDetail {}

#[derive(Serialize)]
struct HarborVerifierResult<'a> {
    rewards: &'a BTreeMap<String, f64>,
}

#[derive(Serialize)]
struct HarborArtifactManifestEntry {}

#[derive(Serialize)]
struct HarborJobResult {
    id: Uuid,
    started_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    n_total_trials: usize,
    stats: HarborJobStats,
}

#[derive(Serialize)]
struct HarborJobStats {
    n_completed_trials: usize,
    n_errored_trials: usize,
    n_running_trials: usize,
    n_pending_trials: usize,
    n_cancelled_trials: usize,
    n_retries: usize,
    evals: BTreeMap<String, HarborAgentDatasetStats>,
    n_input_tokens: u64,
    n_cache_tokens: u64,
    n_output_tokens: u64,
    cost_usd: Option<f64>,
}

#[derive(Serialize)]
struct HarborAgentDatasetStats {
    n_trials: usize,
    n_errors: usize,
    metrics: Vec<HarborMetric>,
    pass_at_k: BTreeMap<usize, f64>,
    reward_stats: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    exception_stats: BTreeMap<String, Vec<String>>,
}

#[derive(Serialize)]
struct HarborMetric {}

#[cfg(test)]
mod tests {
    use std::fs;

    use nanocodex::{Nanocodex, OpenAiAuth};
    use nanoeval::{AtifTrajectory, Nanoeval, Task};
    use serde::Deserialize;
    use tempfile::tempdir;

    use super::Harbor;

    #[derive(Deserialize)]
    struct TrialResult {
        exception_info: Option<ExceptionInfo>,
    }

    #[derive(Deserialize)]
    struct ExceptionInfo {
        exception_type: String,
        exception_message: String,
    }

    #[derive(Deserialize)]
    struct JobResult {
        n_total_trials: usize,
        stats: JobStats,
    }

    #[derive(Deserialize)]
    struct JobStats {
        #[serde(rename = "n_completed_trials")]
        completed: usize,
        #[serde(rename = "n_errored_trials")]
        errored: usize,
        #[serde(rename = "n_pending_trials")]
        pending: usize,
    }

    #[tokio::test]
    async fn records_an_errored_attempt_as_a_harbor_trial() {
        let task_root = tempdir().unwrap();
        fs::create_dir(task_root.path().join("tests")).unwrap();
        fs::create_dir(task_root.path().join("environment")).unwrap();
        fs::write(
            task_root.path().join("task.toml"),
            r#"
schema_version = "1.1"
[task]
name = "terminal-bench/errored"
description = "Errored Harbor fixture"
[metadata]
custom_docker_compose = true
[agent]
timeout_sec = 1.0
[verifier]
timeout_sec = 1.0
[environment]
docker_image = "example/errored:latest"
cpus = 1
memory_mb = 128
storage_mb = 128
gpus = 0
allow_internet = false
"#,
        )
        .unwrap();
        fs::write(task_root.path().join("instruction.md"), "do the work\n").unwrap();
        fs::write(task_root.path().join("tests/test.sh"), "exit 0\n").unwrap();
        let task = Task::load(task_root.path()).unwrap();
        let output = tempdir().unwrap();
        let (eval, events) = Nanoeval::builder(Nanocodex::builder(OpenAiAuth::api_key("test")))
            .output_directory(output.path())
            .build()
            .unwrap();
        let recorder = Harbor::new(&eval)
            .unwrap()
            .record(events.subscribe())
            .unwrap();

        assert!(eval.task(task).await.is_err());
        let job = recorder.finish_all(1).await.unwrap();
        let trial = fs::read_dir(job.directory())
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .unwrap()
            .path();
        let result: TrialResult =
            serde_json::from_slice(&fs::read(trial.join("result.json")).unwrap()).unwrap();
        let exception = result.exception_info.unwrap();
        assert_eq!(exception.exception_type, "EnvironmentError");
        assert!(
            exception
                .exception_message
                .contains("custom Docker Compose")
        );
        serde_json::from_slice::<AtifTrajectory>(
            &fs::read(trial.join("agent/trajectory.json")).unwrap(),
        )
        .unwrap();

        let result: JobResult =
            serde_json::from_slice(&fs::read(job.directory().join("result.json")).unwrap())
                .unwrap();
        assert_eq!(result.n_total_trials, 1);
        assert_eq!(result.stats.completed, 1);
        assert_eq!(result.stats.errored, 1);
        assert_eq!(result.stats.pending, 0);
    }
}

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

use chrono::{DateTime, Utc};
use serde::Serialize;
use url::Url;
use uuid::Uuid;

use crate::{
    AgentMetadata, EvalError, EvalResult, PhaseTiming, Task,
    harbor_checksum::{directory_hash, package_content_hash},
    native::NativeAttempt,
};

/// Owns one Harbor-shaped retained job directory.
pub(crate) struct HarborArtifacts {
    job_id: Uuid,
    started_at: DateTime<Utc>,
    root: PathBuf,
    jobs_dir: PathBuf,
    max_concurrency: usize,
    recorded_trials: Mutex<Vec<HarborRecordedTrial>>,
}

impl HarborArtifacts {
    pub fn create(output_directory: &Path, max_concurrency: usize) -> Result<Self, EvalError> {
        let job_id = Uuid::new_v4();
        fs::create_dir_all(output_directory)?;
        let output_directory = fs::canonicalize(output_directory)?;
        let root = output_directory.join(job_id.to_string());
        fs::create_dir_all(&root)?;
        let artifacts = Self {
            job_id,
            started_at: Utc::now(),
            root,
            jobs_dir: output_directory,
            max_concurrency,
            recorded_trials: Mutex::new(Vec::new()),
        };
        Self::write_file(&artifacts.root.join("job.log"), [])?;
        artifacts.write_job_metadata()?;
        artifacts.write_job(&[])?;
        Ok(artifacts)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn trial_name(task: &Task, attempt_id: Uuid) -> String {
        let short_name = task.name().rsplit('/').next().unwrap_or(task.name());
        let compact_id = attempt_id.simple().to_string();
        format!("{short_name}__{}", &compact_id[..8])
    }

    pub fn write_input(attempt: &NativeAttempt, task: &Task) -> Result<(), EvalError> {
        let input = HarborInput {
            protocol_version: 1,
            request_id: attempt
                .paths
                .root
                .file_name()
                .and_then(|name| name.to_str()),
            kind: "input",
            payload: HarborInputPayload {
                instruction: task.prompt(),
            },
        };
        let mut bytes = serde_json::to_vec(&input)?;
        bytes.push(b'\n');
        Self::write_file(&attempt.paths.agent.join("input.jsonl"), bytes)
    }

    pub fn write_trial(
        &self,
        attempt: &NativeAttempt,
        task: &Task,
        result: &EvalResult,
    ) -> Result<(), EvalError> {
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
            agent: HarborAgentConfig {
                name: "nanocodex",
                model_name: format!("openai/{}", result.agent.model),
                kwargs: HarborAgentKwargs {
                    effort: result.agent.effort.clone(),
                },
            },
            environment: HarborEnvironmentConfig::native(),
            verifier: HarborVerifierConfig::native(),
            artifacts: Vec::new(),
            extra_instruction_paths: Vec::new(),
            job_id: self.job_id,
        };
        Self::write_json(&attempt.paths.root.join("config.json"), &config)?;
        Self::write_json(&attempt.paths.trajectory, &result.trajectory)?;
        Self::write_json(
            &attempt.paths.root.join("artifacts/manifest.json"),
            &Vec::<HarborArtifactManifestEntry>::new(),
        )?;

        let trial_uri = Url::from_directory_path(&attempt.paths.root)
            .map_err(|()| EvalError::InvalidTrialPath(attempt.paths.root.clone()))?
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
            agent_result: HarborAgentResult {
                n_input_tokens: result.agent.usage.input_tokens,
                n_cache_tokens: result.agent.usage.cached_input_tokens,
                n_output_tokens: result.agent.usage.output_tokens,
                cost_usd: result.agent.cost_usd,
                rollout_details: None,
                metadata: &result.agent.metadata,
            },
            verifier_result: HarborVerifierResult {
                rewards: &result.verifier.rewards,
            },
            started_at: result.timing.started_at,
            finished_at: result.timing.finished_at,
            environment_setup: &result.timing.environment_setup,
            agent_setup: &result.timing.agent_setup,
            agent_execution: &result.timing.agent_execution,
            verifier: &result.timing.verifier,
            exception_info: None,
            step_results: None,
        };
        Self::write_json(&attempt.paths.result, &trial_result)?;
        Self::write_file(&attempt.paths.root.join("trial.log"), [])?;
        Self::write_file(&attempt.paths.agent.join("stderr.log"), [])?;

        let lock = HarborTrialLock::new(task, result, &task_digest);
        Self::write_json(&attempt.paths.root.join("lock.json"), &lock)?;
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
                agent: HarborAgentConfig {
                    name: "nanocodex",
                    model_name: format!("openai/{}", result.agent.model),
                    kwargs: HarborAgentKwargs {
                        effort: result.agent.effort.clone(),
                    },
                },
                lock,
            });
        }
        self.write_job_metadata()
    }

    pub fn write_job(&self, results: &[EvalResult]) -> Result<(), EvalError> {
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
            || "nanocodex__nanoeval/local".to_owned(),
            |result| format!("nanocodex__{}__nanoeval/local", result.agent.model),
        );
        let eval_stats = HarborAgentDatasetStats {
            n_trials: results.len(),
            n_errors: 0,
            metrics: Vec::new(),
            pass_at_k: BTreeMap::new(),
            reward_stats,
            exception_stats: BTreeMap::new(),
        };
        let job = HarborJobResult {
            id: self.job_id,
            started_at: self.started_at,
            updated_at: now,
            finished_at: (!results.is_empty()).then_some(now),
            n_total_trials: results.len(),
            stats: HarborJobStats {
                n_completed_trials: results.len(),
                n_errored_trials: 0,
                n_running_trials: 0,
                n_pending_trials: 0,
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

    fn write_job_metadata(&self) -> Result<(), EvalError> {
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

    fn write_json(path: &Path, value: &impl Serialize) -> Result<(), EvalError> {
        let mut bytes = serde_json::to_vec_pretty(value)?;
        bytes.push(b'\n');
        Self::atomic_write(path, bytes)
    }

    fn atomic_write(path: &Path, bytes: impl AsRef<[u8]>) -> Result<(), EvalError> {
        let mut name: OsString = path
            .file_name()
            .map_or_else(|| OsString::from("artifact"), OsString::from);
        name.push(format!(".{}.tmp", Uuid::new_v4()));
        let temporary = path.with_file_name(name);
        Self::write_file(&temporary, bytes)?;
        fs::rename(&temporary, path)?;
        Ok(())
    }

    fn write_file(path: &Path, bytes: impl AsRef<[u8]>) -> Result<(), EvalError> {
        fs::write(path, bytes)?;
        Ok(())
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
    request_id: Option<&'a str>,
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
    fn new(task: &Task, result: &EvalResult, digest: &str) -> Self {
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
                model_name: format!("openai/{}", result.agent.model),
                kwargs: HarborAgentKwargs {
                    effort: result.agent.effort.clone(),
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
    agent_result: HarborAgentResult<'a>,
    verifier_result: HarborVerifierResult<'a>,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    environment_setup: &'a PhaseTiming,
    agent_setup: &'a PhaseTiming,
    agent_execution: &'a PhaseTiming,
    verifier: &'a PhaseTiming,
    exception_info: Option<HarborExceptionInfo>,
    step_results: Option<Vec<HarborStepResult>>,
}

#[derive(Serialize)]
struct HarborExceptionInfo {}

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

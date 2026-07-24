use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    time::Instant,
};

use clap::Args;
use eyre::{Context, Result, bail, eyre};
use nanoeval_harbor::{
    PublishedAgentInfo, PublishedAttempts, PublishedQuery, PublishedResults, PublishedTask,
    PublishedTrajectory, PublishedTrial,
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use tokio::task::JoinSet;

#[derive(Args)]
pub(crate) struct Compare {
    /// Terminal-Bench task name or retained Nanoeval Harbor job directory.
    #[arg(value_name = "TASK_OR_JOB")]
    target: PathBuf,

    /// Prefer and identify attempts for this exact Harbor task checksum.
    #[arg(long, value_name = "SHA256")]
    checksum: Option<String>,

    /// Select one task by literal name when comparing a retained job.
    #[arg(long, value_name = "TEXT")]
    task: Option<String>,

    /// Return at most one successful attempt from this many harness submissions.
    #[arg(long, default_value_t = 10)]
    limit: usize,

    /// Match a harness, agent, or model name. Repeat to match any value.
    #[arg(long, value_name = "TEXT")]
    agent: Vec<String>,

    /// Drill into one published submission, harness, or model after ranking.
    #[arg(long, value_name = "TEXT")]
    show: Option<String>,

    /// Select one retained local trial by name or substring for trajectory comparison.
    #[arg(long, value_name = "TEXT", requires = "show")]
    local_trial: Option<String>,

    /// Content-addressed cache for the public archive index and downloaded artifacts.
    #[arg(long, default_value = ".cache/nanoeval/published")]
    cache: PathBuf,

    /// Refresh the public archive tree index before querying.
    #[arg(long)]
    refresh: bool,

    /// Emit the complete typed comparison report as JSON.
    #[arg(long)]
    json: bool,

    /// Include reasoning and complete tool observations in human-readable output.
    #[arg(long, conflicts_with = "json")]
    full: bool,
}

impl Compare {
    pub(crate) async fn run(self) -> Result<()> {
        if self.target.is_dir() {
            self.run_job().await
        } else {
            self.run_task().await
        }
    }

    async fn run_task(&self) -> Result<()> {
        if self.task.is_some() {
            bail!("--task selects a task inside a retained job directory");
        }
        let task = self
            .target
            .to_str()
            .ok_or_else(|| eyre!("task name is not valid UTF-8"))?;
        let mut query = PublishedQuery::new(task).limit(self.limit);
        if let Some(checksum) = &self.checksum {
            query = query.checksum(checksum);
        }
        for agent in &self.agent {
            query = query.agent(agent);
        }
        let published = self.published()?;
        let report = published.query(&query).await?;

        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        if self.json {
            serde_json::to_writer_pretty(&mut stdout, &report)?;
            writeln!(stdout)?;
        } else {
            write_human(&mut stdout, &report, self.full)?;
        }
        Ok(())
    }

    async fn run_job(&self) -> Result<()> {
        if self.checksum.is_some() {
            bail!("--checksum cannot be used with a retained job; its trial checksums are used");
        }
        if self.full && self.show.is_none() {
            bail!("--full requires --show when comparing a retained job");
        }
        let started = Instant::now();
        let mut local = LocalJob::load(&self.target)?;
        if let Some(task) = &self.task {
            local.select_task(task)?;
        }
        if self.show.is_some() && local.tasks.len() != 1 {
            bail!("--show requires a one-task job or --task selection");
        }
        let published = self.published()?;
        let mut queries = JoinSet::new();
        for task in local.tasks.values() {
            let published = published.clone();
            let mut query = PublishedQuery::new(&task.task_name).checksum(&task.task_checksum);
            for agent in &self.agent {
                query = query.agent(agent);
            }
            queries.spawn(async move { published.attempts(&query).await });
        }
        let mut published_tasks = Vec::with_capacity(local.tasks.len());
        while let Some(result) = queries.join_next().await {
            published_tasks.push(result.context("published comparison task stopped")??);
        }
        let drilldown = match &self.show {
            Some(selector) => {
                let task = local.single_task()?;
                let submission = resolve_show_selector(&published_tasks, selector)?;
                let query = PublishedQuery::new(&task.task_name)
                    .checksum(&task.task_checksum)
                    .agent(&submission)
                    .limit(1);
                let selected = published.query(&query).await?;
                Some(TrajectoryComparison::new(
                    &local,
                    selected,
                    self.local_trial.as_deref(),
                )?)
            }
            None => None,
        };
        let report = JobComparison::new(
            self.target.canonicalize()?,
            local,
            published_tasks,
            self.limit,
            started.elapsed().as_secs_f64(),
            drilldown,
        )?;

        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        if self.json {
            serde_json::to_writer_pretty(&mut stdout, &report)?;
            writeln!(stdout)?;
        } else {
            report.write_human(&mut stdout, self.full)?;
        }
        Ok(())
    }

    fn published(&self) -> Result<PublishedResults> {
        Ok(PublishedResults::builder()
            .cache_directory(&self.cache)
            .refresh(self.refresh)
            .build()?)
    }
}

fn resolve_show_selector(published_tasks: &[PublishedAttempts], selector: &str) -> Result<String> {
    let selector = selector.to_lowercase();
    let matches = published_tasks
        .iter()
        .flat_map(|task| &task.attempts)
        .filter(|attempt| {
            [
                Some(attempt.submission.as_str()),
                Some(attempt.agent.name.as_str()),
                attempt.agent.version.as_deref(),
                attempt
                    .agent
                    .model_info
                    .as_ref()
                    .map(|model| model.name.as_str()),
                attempt
                    .agent
                    .model_info
                    .as_ref()
                    .and_then(|model| model.provider.as_deref()),
                attempt.agent_import_path.as_deref(),
            ]
            .into_iter()
            .flatten()
            .any(|value| value.to_lowercase().contains(&selector))
        })
        .map(|attempt| attempt.submission.clone())
        .collect::<BTreeSet<_>>();
    match matches.len() {
        0 => bail!("--show {selector:?} matched no published submission"),
        1 => matches
            .into_iter()
            .next()
            .ok_or_else(|| eyre!("published submission selector matched no result")),
        count => bail!(
            "--show {selector:?} matched {count} submissions; use one exact submission name: {}",
            matches.into_iter().collect::<Vec<_>>().join(", ")
        ),
    }
}

#[derive(Deserialize)]
struct LocalTrial {
    trial_name: String,
    task_name: String,
    task_checksum: String,
    agent_info: LocalAgentInfo,
    config: Option<LocalTrialConfig>,
    verifier_result: Option<LocalVerifierResult>,
    exception_info: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
struct LocalAgentInfo {
    name: String,
    version: Option<String>,
    model_info: Option<LocalModelInfo>,
}

#[derive(Deserialize)]
struct LocalModelInfo {
    name: String,
}

#[derive(Deserialize)]
struct LocalTrialConfig {
    agent: LocalAgentConfig,
}

#[derive(Deserialize)]
struct LocalAgentConfig {
    kwargs: LocalAgentKwargs,
}

#[derive(Deserialize)]
struct LocalAgentKwargs {
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
}

#[derive(Deserialize)]
struct LocalVerifierResult {
    rewards: BTreeMap<String, f64>,
}

#[derive(Clone, Debug, Serialize)]
struct TaskScore {
    task_name: String,
    task_checksum: String,
    trials: usize,
    passes: usize,
    errors: usize,
    trajectories: usize,
}

impl TaskScore {
    const fn pass_at_k(&self) -> bool {
        self.passes > 0
    }
}

struct LocalJob {
    agent: String,
    agent_version: Option<String>,
    model: String,
    thinking_configured: BTreeSet<String>,
    thinking_observed: BTreeSet<String>,
    thinking_observed_trials: usize,
    tasks: BTreeMap<String, TaskScore>,
    trials: BTreeMap<String, Vec<LocalAttempt>>,
}

#[derive(Default)]
struct LocalJobLoader {
    tasks: BTreeMap<String, TaskScore>,
    trials: BTreeMap<String, Vec<LocalAttempt>>,
    agents: BTreeSet<(String, Option<String>, String)>,
    thinking_configured: BTreeSet<String>,
    thinking_observed: BTreeSet<String>,
    thinking_observed_trials: usize,
}

#[derive(Clone, Debug, Serialize)]
struct LocalAttempt {
    directory: PathBuf,
    trial_name: String,
    passed: bool,
    configured_thinking: Option<String>,
    observed_thinking: Option<String>,
}

impl LocalJob {
    fn load(job: &Path) -> Result<Self> {
        if !job.join("result.json").is_file() {
            bail!("retained Harbor job is incomplete: {}", job.display());
        }
        let mut loader = LocalJobLoader::default();
        for entry in fs::read_dir(job)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            if let Some(trial) = load_local_trial(&entry.path())? {
                loader.record(job, entry.path(), trial)?;
            }
        }
        loader.finish(job)
    }

    fn select_task(&mut self, needle: &str) -> Result<()> {
        let matches = self
            .tasks
            .keys()
            .filter(|task| task.contains(needle))
            .cloned()
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            bail!(
                "task selector {needle:?} matched {} tasks: {}",
                matches.len(),
                matches.join(", ")
            );
        }
        let task = matches
            .first()
            .ok_or_else(|| eyre!("task selector matched no task"))?;
        self.tasks.retain(|name, _| name == task);
        self.trials.retain(|name, _| name == task);
        self.thinking_configured = self
            .trials
            .values()
            .flatten()
            .filter_map(|attempt| attempt.configured_thinking.clone())
            .collect();
        self.thinking_observed = self
            .trials
            .values()
            .flatten()
            .filter_map(|attempt| attempt.observed_thinking.clone())
            .collect();
        self.thinking_observed_trials = self
            .trials
            .values()
            .flatten()
            .filter(|attempt| attempt.observed_thinking.is_some())
            .count();
        Ok(())
    }

    fn single_task(&self) -> Result<&TaskScore> {
        if self.tasks.len() != 1 {
            bail!("trajectory comparison requires exactly one selected task");
        }
        self.tasks
            .values()
            .next()
            .ok_or_else(|| eyre!("retained job contains no selected task"))
    }

    fn select_attempt(&self, selector: Option<&str>) -> Result<&LocalAttempt> {
        let task = self.single_task()?;
        let attempts = self
            .trials
            .get(&task.task_name)
            .ok_or_else(|| eyre!("selected task contains no local attempts"))?;
        if let Some(selector) = selector {
            let matches = attempts
                .iter()
                .filter(|attempt| attempt.trial_name.contains(selector))
                .collect::<Vec<_>>();
            if matches.len() != 1 {
                bail!(
                    "local trial selector {selector:?} matched {} trials: {}",
                    matches.len(),
                    matches
                        .iter()
                        .map(|attempt| attempt.trial_name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            return matches
                .into_iter()
                .next()
                .ok_or_else(|| eyre!("local trial selector matched no trial"));
        }
        attempts
            .iter()
            .find(|attempt| !attempt.passed)
            .or_else(|| attempts.first())
            .ok_or_else(|| eyre!("selected task contains no local attempts"))
    }
}

fn load_local_trial(directory: &Path) -> Result<Option<LocalTrial>> {
    let result = directory.join("result.json");
    let bytes = match fs::read(&result) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    serde_json::from_slice(&bytes)
        .wrap_err_with(|| format!("failed to decode {}", result.display()))
        .map(Some)
}

impl LocalJobLoader {
    fn record(&mut self, job: &Path, directory: PathBuf, trial: LocalTrial) -> Result<()> {
        let configured_thinking = trial.config.as_ref().and_then(|config| {
            config
                .agent
                .kwargs
                .effort
                .clone()
                .or_else(|| config.agent.kwargs.reasoning_effort.clone())
        });
        if let Some(thinking) = &configured_thinking {
            self.thinking_configured.insert(thinking.clone());
        }
        let observed_thinking = load_observed_thinking(&directory)?;
        if let Some(thinking) = &observed_thinking {
            self.thinking_observed.insert(thinking.clone());
            self.thinking_observed_trials += 1;
        }
        self.agents.insert((
            trial.agent_info.name,
            trial.agent_info.version,
            trial
                .agent_info
                .model_info
                .map_or_else(|| "unknown".to_owned(), |model| model.name),
        ));
        let passed = trial.exception_info.is_none()
            && trial
                .verifier_result
                .as_ref()
                .and_then(|verifier| verifier.rewards.get("reward"))
                .is_some_and(|reward| *reward > 0.0);
        let score = self
            .tasks
            .entry(trial.task_name.clone())
            .or_insert_with(|| TaskScore {
                task_name: trial.task_name.clone(),
                task_checksum: trial.task_checksum.clone(),
                trials: 0,
                passes: 0,
                errors: 0,
                trajectories: 0,
            });
        if score.task_checksum != trial.task_checksum {
            bail!(
                "task {} has multiple checksums in {}",
                score.task_name,
                job.display()
            );
        }
        score.trials += 1;
        score.passes += usize::from(passed);
        score.errors += usize::from(trial.exception_info.is_some());
        score.trajectories += usize::from(directory.join("agent/trajectory.json").is_file());
        self.trials
            .entry(trial.task_name)
            .or_default()
            .push(LocalAttempt {
                directory,
                trial_name: trial.trial_name,
                passed,
                configured_thinking,
                observed_thinking,
            });
        Ok(())
    }

    fn finish(mut self, job: &Path) -> Result<LocalJob> {
        if self.tasks.is_empty() {
            bail!(
                "retained job contains no completed trials: {}",
                job.display()
            );
        }
        if self.agents.len() != 1 {
            bail!(
                "job comparison currently requires one agent/model pair; found {}",
                self.agents.len()
            );
        }
        let (agent, agent_version, model) = self
            .agents
            .pop_first()
            .ok_or_else(|| eyre!("retained job contains no agent metadata"))?;
        Ok(LocalJob {
            agent,
            agent_version,
            model,
            thinking_configured: self.thinking_configured,
            thinking_observed: self.thinking_observed,
            thinking_observed_trials: self.thinking_observed_trials,
            tasks: self.tasks,
            trials: self.trials,
        })
    }
}

#[derive(Deserialize)]
struct EventKind {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct RunStartedEvent {
    #[serde(rename = "type")]
    kind: String,
    payload: RunStartedPayload,
}

#[derive(Deserialize)]
struct RunStartedPayload {
    effort: String,
}

fn load_observed_thinking(trial: &Path) -> Result<Option<String>> {
    let events = trial.join("agent/events.jsonl");
    let file = match File::open(&events) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    for line in BufReader::new(file).lines() {
        let line = line?;
        let kind: EventKind = serde_json::from_str(&line)
            .wrap_err_with(|| format!("failed to decode event kind in {}", events.display()))?;
        if kind.kind != "run.started" {
            continue;
        }
        let event: RunStartedEvent = serde_json::from_str(&line)
            .wrap_err_with(|| format!("failed to decode run.started in {}", events.display()))?;
        if event.kind != "run.started" {
            bail!("decoded unexpected event kind {}", event.kind);
        }
        return Ok(Some(event.payload.effort));
    }
    Ok(None)
}

#[derive(Debug, Serialize)]
struct TrajectoryComparison {
    local: ComparedTrajectory,
    published: ComparedTrajectory,
}

#[derive(Debug, Serialize)]
struct ComparedTrajectory {
    label: String,
    trial_name: String,
    task_checksum: String,
    passed: bool,
    configured_thinking: Option<String>,
    observed_thinking: Option<String>,
    verifier_evidence: Option<String>,
    trajectory_error: Option<String>,
    trajectory: Option<PublishedTrajectory>,
}

impl TrajectoryComparison {
    fn new(
        local: &LocalJob,
        published: PublishedTask,
        local_selector: Option<&str>,
    ) -> Result<Self> {
        let local_attempt = local.select_attempt(local_selector)?;
        let local_trajectory_path = local_attempt.directory.join("agent/trajectory.json");
        let local_trajectory =
            serde_json::from_slice::<PublishedTrajectory>(&fs::read(&local_trajectory_path)?)
                .wrap_err_with(|| {
                    format!(
                        "failed to decode local trajectory {}",
                        local_trajectory_path.display()
                    )
                })?;
        let published_trial = published.trials.into_iter().next().ok_or_else(|| {
            eyre!("--show matched no successful published attempt with a retained result")
        })?;
        let model = published_trial
            .agent
            .model_info
            .as_ref()
            .map_or("unknown model", |model| model.name.as_str());
        let import_path = published_trial
            .agent_import_path
            .as_deref()
            .map_or(String::new(), |path| format!(" · {path}"));
        Ok(Self {
            local: ComparedTrajectory {
                label: format!("nanoeval · {} · {}", local.agent, local.model),
                trial_name: local_attempt.trial_name.clone(),
                task_checksum: local.single_task()?.task_checksum.clone(),
                passed: local_attempt.passed,
                configured_thinking: local_attempt.configured_thinking.clone(),
                observed_thinking: local_attempt.observed_thinking.clone(),
                verifier_evidence: load_verifier_evidence(&local_attempt.directory)?,
                trajectory_error: None,
                trajectory: Some(local_trajectory),
            },
            published: ComparedTrajectory {
                label: format!(
                    "{} · {} · {model}{import_path}",
                    published_trial.submission, published_trial.agent.name
                ),
                trial_name: published_trial.trial_name,
                task_checksum: published_trial.task_checksum,
                passed: published_trial.reward > 0.0,
                configured_thinking: published_trial.thinking,
                observed_thinking: None,
                verifier_evidence: None,
                trajectory_error: published_trial.trajectory_error,
                trajectory: published_trial.trajectory,
            },
        })
    }

    fn write_human(&self, output: &mut impl Write, full: bool) -> io::Result<()> {
        writeln!(output, "\nTrajectory comparison:")?;
        write_trajectory_header(output, &self.local, true)?;
        write_trajectory_header(output, &self.published, false)?;
        writeln!(output, "\nStructural diff:")?;
        write_structural_diff(output, &self.local, &self.published)?;
        writeln!(output, "\nLocal timeline:")?;
        write_compared_timeline(output, &self.local, full)?;
        writeln!(output, "\nPublished timeline:")?;
        write_compared_timeline(output, &self.published, full)?;
        Ok(())
    }
}

fn write_trajectory_header(
    output: &mut impl Write,
    trajectory: &ComparedTrajectory,
    local: bool,
) -> io::Result<()> {
    let outcome = if trajectory.passed { "pass" } else { "fail" };
    let thinking = match (
        trajectory.configured_thinking.as_deref(),
        trajectory.observed_thinking.as_deref(),
    ) {
        (Some(configured), Some(observed)) if configured == observed => {
            format!("{configured} (runtime verified)")
        }
        (Some(configured), Some(observed)) => {
            format!("configured {configured}, observed {observed}")
        }
        (Some(configured), None) if local => format!("{configured} (not observed)"),
        (Some(configured), None) => format!("{configured} (published config)"),
        (None, _) => "not published".to_owned(),
    };
    writeln!(
        output,
        "{} · {} · {outcome} · thinking {thinking} · checksum {}",
        trajectory.label, trajectory.trial_name, trajectory.task_checksum
    )?;
    if let Some(evidence) = &trajectory.verifier_evidence {
        writeln!(output, "  verifier: {evidence}")?;
    }
    Ok(())
}

fn write_structural_diff(
    output: &mut impl Write,
    local: &ComparedTrajectory,
    published: &ComparedTrajectory,
) -> io::Result<()> {
    let local_metrics = local.trajectory.as_ref().map(TrajectoryMetrics::new);
    let published_metrics = published.trajectory.as_ref().map(TrajectoryMetrics::new);
    match (local_metrics, published_metrics) {
        (Some(local), Some(published)) => {
            writeln!(
                output,
                "  steps · local {} vs published {}",
                local.steps, published.steps
            )?;
            writeln!(
                output,
                "  agent turns · local {} vs published {}",
                local.agent_turns, published.agent_turns
            )?;
            writeln!(
                output,
                "  tool calls · local {} [{}] vs published {} [{}]",
                local.tool_calls,
                local.tools.join(", "),
                published.tool_calls,
                published.tools.join(", ")
            )?;
        }
        _ => writeln!(output, "  one side has no published trajectory to diff")?,
    }
    Ok(())
}

struct TrajectoryMetrics {
    steps: usize,
    agent_turns: usize,
    tool_calls: usize,
    tools: Vec<String>,
}

impl TrajectoryMetrics {
    fn new(trajectory: &PublishedTrajectory) -> Self {
        let mut tools = BTreeSet::new();
        let mut tool_calls = 0;
        for step in &trajectory.steps {
            for call in &step.tool_calls {
                tools.insert(call.function_name.clone());
                tool_calls += 1;
            }
        }
        Self {
            steps: trajectory.steps.len(),
            agent_turns: trajectory
                .steps
                .iter()
                .filter(|step| step.source == "agent")
                .count(),
            tool_calls,
            tools: tools.into_iter().collect(),
        }
    }
}

fn write_compared_timeline(
    output: &mut impl Write,
    compared: &ComparedTrajectory,
    full: bool,
) -> io::Result<()> {
    let Some(trajectory) = &compared.trajectory else {
        return writeln!(
            output,
            "  unavailable{}",
            compared
                .trajectory_error
                .as_deref()
                .map_or(String::new(), |error| format!(": {error}"))
        );
    };
    for step in &trajectory.steps {
        if step.source == "user" && !full {
            continue;
        }
        if full && let Some(reasoning) = nonempty(step.reasoning_content.as_deref()) {
            writeln!(output, "  {} reasoning: {reasoning}", step.source)?;
        }
        if let Some(message) = nonempty(step.message.as_deref())
            && (full || step.source == "agent" || step.tool_calls.is_empty())
        {
            writeln!(output, "  {}: {}", step.source, one_line(message, full))?;
        }
        for call in &step.tool_calls {
            writeln!(
                output,
                "  tool {} {}",
                call.function_name,
                one_line(call.arguments.get(), full)
            )?;
        }
        if full && let Some(observation) = &step.observation {
            for result in &observation.results {
                writeln!(
                    output,
                    "  result {}: {}",
                    result.source_call_id, result.content
                )?;
            }
        }
    }
    Ok(())
}

fn load_verifier_evidence(trial: &Path) -> Result<Option<String>> {
    let output = trial.join("verifier/test-stdout.txt");
    let contents = match fs::read_to_string(&output) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let evidence = contents
        .lines()
        .map(str::trim)
        .filter(|line| {
            line.starts_with("FAILED ")
                || line.starts_with("ERROR ")
                || line.starts_with("E ")
                    && (line.contains("Error")
                        || line.contains("Exception")
                        || line.contains("assert"))
        })
        .take(3)
        .collect::<Vec<_>>()
        .join(" · ");
    Ok((!evidence.is_empty()).then_some(evidence))
}

#[derive(Debug, Serialize)]
struct JobComparison {
    job_directory: PathBuf,
    archive_revision: String,
    lookup_seconds: f64,
    local: RunScore,
    exact: Vec<RunScore>,
    other_revisions: Vec<RunScore>,
    drilldown: Option<TrajectoryComparison>,
}

#[derive(Clone, Debug, Serialize)]
struct RunScore {
    submission: String,
    runs: Vec<String>,
    agent: String,
    agent_version: Option<String>,
    agent_import_paths: Vec<String>,
    model: String,
    thinking: ThinkingEvidence,
    exact_revision: bool,
    tasks: usize,
    k: Option<usize>,
    pass_at_k_tasks: usize,
    trials: usize,
    passes: usize,
    errors: usize,
    trajectories: usize,
    task_scores: Vec<TaskScore>,
}

struct RunScoreInput {
    submission: String,
    runs: Vec<String>,
    agent: String,
    agent_version: Option<String>,
    agent_import_paths: Vec<String>,
    model: String,
    thinking: ThinkingEvidence,
    exact_revision: bool,
    task_scores: Vec<TaskScore>,
}

#[derive(Default)]
struct PublishedRun {
    agent: Option<PublishedAgentInfo>,
    thinking: BTreeSet<Option<String>>,
    import_paths: BTreeSet<String>,
    runs: BTreeSet<String>,
    tasks: BTreeMap<String, TaskScore>,
}

#[derive(Clone, Debug, Serialize)]
struct ThinkingEvidence {
    configured: Vec<String>,
    observed: Vec<String>,
    observed_trials: usize,
    total_trials: usize,
    provenance: ThinkingProvenance,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ThinkingProvenance {
    RuntimeEvents,
    PublishedConfig,
}

impl JobComparison {
    fn new(
        job_directory: PathBuf,
        local: LocalJob,
        published_tasks: Vec<PublishedAttempts>,
        limit: usize,
        lookup_seconds: f64,
        drilldown: Option<TrajectoryComparison>,
    ) -> Result<Self> {
        let revisions = published_tasks
            .iter()
            .map(|task| task.archive_revision.as_str())
            .collect::<BTreeSet<_>>();
        if revisions.len() != 1 {
            bail!("published archive changed during the comparison; retry the command");
        }
        let archive_revision = revisions
            .first()
            .ok_or_else(|| eyre!("published comparison returned no tasks"))?
            .to_string();
        let expected_tasks = local.tasks.len();
        let local_score = RunScore::local(
            local.agent,
            local.agent_version,
            local.model,
            local.thinking_configured,
            local.thinking_observed,
            local.thinking_observed_trials,
            local.tasks.values().cloned().collect::<Vec<_>>(),
        );
        let one_task = expected_tasks == 1;
        let mut exact_runs = BTreeMap::<String, PublishedRun>::new();
        let mut other_runs = BTreeMap::<(String, String), PublishedRun>::new();
        for task in published_tasks {
            let checksum = task
                .requested_checksum
                .as_deref()
                .ok_or_else(|| eyre!("published task query lost its checksum"))?;
            for attempt in task.attempts {
                if attempt.task_checksum == checksum {
                    exact_runs
                        .entry(attempt.submission.clone())
                        .or_default()
                        .record(attempt);
                } else if one_task {
                    other_runs
                        .entry((attempt.submission.clone(), attempt.task_checksum.clone()))
                        .or_default()
                        .record(attempt);
                }
            }
        }
        let mut exact = exact_runs
            .into_iter()
            .filter(|(_, run)| run.tasks.len() == expected_tasks)
            .map(|(submission, run)| RunScore::published(submission, run, true))
            .collect::<Vec<_>>();
        sort_runs(&mut exact);
        exact.truncate(limit);
        let mut other_revisions = other_runs
            .into_iter()
            .map(|((submission, _), run)| RunScore::published(submission, run, false))
            .collect::<Vec<_>>();
        sort_runs(&mut other_revisions);
        other_revisions.truncate(limit);
        Ok(Self {
            job_directory,
            archive_revision,
            lookup_seconds,
            local: local_score,
            exact,
            other_revisions,
            drilldown,
        })
    }

    fn write_human(&self, output: &mut impl Write, full: bool) -> io::Result<()> {
        writeln!(
            output,
            "Harbor comparison for {}",
            self.job_directory.display()
        )?;
        writeln!(
            output,
            "{} tasks · archive {} · lookup {:.3}s",
            self.local.tasks,
            &self.archive_revision[..self.archive_revision.len().min(12)],
            self.lookup_seconds
        )?;
        write_run_score(output, &self.local)?;
        if self.exact.is_empty() {
            writeln!(
                output,
                "\nNo published run covers every exact task revision in this job."
            )?;
        } else {
            writeln!(output, "\nExact-revision published runs:")?;
            for run in &self.exact {
                write_run_score(output, run)?;
            }
            self.write_task_comparison(output, &self.exact[0])?;
        }
        if !self.other_revisions.is_empty() {
            writeln!(
                output,
                "\nSame task name, other published revisions (not checksum-identical):"
            )?;
            for run in &self.other_revisions {
                write_run_score(output, run)?;
            }
        }
        if let Some(drilldown) = &self.drilldown {
            drilldown.write_human(output, full)?;
        }
        Ok(())
    }

    fn write_task_comparison(
        &self,
        output: &mut impl Write,
        published: &RunScore,
    ) -> io::Result<()> {
        writeln!(
            output,
            "\nPer-task trials (local vs {}):",
            published.submission
        )?;
        for local in &self.local.task_scores {
            let Some(other) = published
                .task_scores
                .iter()
                .find(|task| task.task_name == local.task_name)
            else {
                continue;
            };
            writeln!(
                output,
                "{} · {}/{} vs {}/{}",
                local
                    .task_name
                    .strip_prefix("terminal-bench/")
                    .unwrap_or(&local.task_name),
                local.passes,
                local.trials,
                other.passes,
                other.trials
            )?;
        }
        Ok(())
    }
}

fn sort_runs(runs: &mut [RunScore]) {
    runs.sort_by(|left, right| {
        right
            .pass_at_k_tasks
            .cmp(&left.pass_at_k_tasks)
            .then_with(|| right.passes.cmp(&left.passes))
            .then_with(|| left.errors.cmp(&right.errors))
            .then_with(|| left.submission.cmp(&right.submission))
    });
}

impl PublishedRun {
    fn record(&mut self, attempt: nanoeval_harbor::PublishedAttempt) {
        self.agent.get_or_insert_with(|| attempt.agent.clone());
        self.thinking.insert(attempt.thinking);
        self.runs.insert(attempt.run);
        if let Some(import_path) = attempt.agent_import_path {
            self.import_paths.insert(import_path);
        }
        let score = self
            .tasks
            .entry(attempt.task_name.clone())
            .or_insert_with(|| TaskScore {
                task_name: attempt.task_name,
                task_checksum: attempt.task_checksum,
                trials: 0,
                passes: 0,
                errors: 0,
                trajectories: 0,
            });
        score.trials += 1;
        score.passes += usize::from(attempt.passed);
        score.errors += usize::from(attempt.errored);
        score.trajectories += usize::from(attempt.trajectory_path.is_some());
    }
}

impl RunScore {
    fn local(
        agent: String,
        agent_version: Option<String>,
        model: String,
        thinking_configured: BTreeSet<String>,
        thinking_observed: BTreeSet<String>,
        thinking_observed_trials: usize,
        task_scores: Vec<TaskScore>,
    ) -> Self {
        let total_trials = task_scores.iter().map(|task| task.trials).sum();
        Self::new(RunScoreInput {
            submission: "nanoeval".to_owned(),
            runs: vec!["local".to_owned()],
            agent,
            agent_version,
            agent_import_paths: Vec::new(),
            model,
            thinking: ThinkingEvidence {
                configured: thinking_configured.into_iter().collect(),
                observed: thinking_observed.into_iter().collect(),
                observed_trials: thinking_observed_trials,
                total_trials,
                provenance: ThinkingProvenance::RuntimeEvents,
            },
            exact_revision: true,
            task_scores,
        })
    }

    fn published(submission: String, published: PublishedRun, exact_revision: bool) -> Self {
        let (agent, agent_version, model) = published.agent.map_or_else(
            || ("unknown".to_owned(), None, "unknown".to_owned()),
            |agent| {
                let model = agent
                    .model_info
                    .as_ref()
                    .map_or_else(|| "unknown".to_owned(), |model| model.name.clone());
                (agent.name, agent.version, model)
            },
        );
        let total_trials = published.tasks.values().map(|task| task.trials).sum();
        Self::new(RunScoreInput {
            submission,
            runs: published.runs.into_iter().collect(),
            agent,
            agent_version,
            agent_import_paths: published.import_paths.into_iter().collect(),
            model,
            thinking: ThinkingEvidence {
                configured: published.thinking.into_iter().flatten().collect(),
                observed: Vec::new(),
                observed_trials: 0,
                total_trials,
                provenance: ThinkingProvenance::PublishedConfig,
            },
            exact_revision,
            task_scores: published.tasks.into_values().collect(),
        })
    }

    fn new(input: RunScoreInput) -> Self {
        let RunScoreInput {
            submission,
            runs,
            agent,
            agent_version,
            agent_import_paths,
            model,
            thinking,
            exact_revision,
            task_scores,
        } = input;
        let k = task_scores
            .first()
            .map(|task| task.trials)
            .filter(|trials| task_scores.iter().all(|task| task.trials == *trials));
        Self {
            submission,
            runs,
            agent,
            agent_version,
            agent_import_paths,
            model,
            thinking,
            exact_revision,
            tasks: task_scores.len(),
            k,
            pass_at_k_tasks: task_scores.iter().filter(|task| task.pass_at_k()).count(),
            trials: task_scores.iter().map(|task| task.trials).sum(),
            passes: task_scores.iter().map(|task| task.passes).sum(),
            errors: task_scores.iter().map(|task| task.errors).sum(),
            trajectories: task_scores.iter().map(|task| task.trajectories).sum(),
            task_scores,
        }
    }
}

fn write_run_score(output: &mut impl Write, run: &RunScore) -> io::Result<()> {
    let thinking = run.thinking.human();
    let version = run
        .agent_version
        .as_deref()
        .map_or_else(String::new, |version| format!(" {version}"));
    let trajectory = if run.trajectories == 0 {
        "trajectories none".to_owned()
    } else {
        format!("trajectories {}/{}", run.trajectories, run.trials)
    };
    let errors = if run.errors > 0 {
        format!(" · errors {}", run.errors)
    } else {
        String::new()
    };
    let revision = (!run.exact_revision)
        .then(|| {
            run.task_scores.first().map(|task| {
                format!(
                    " · revision {}",
                    &task.task_checksum[..task.task_checksum.len().min(12)]
                )
            })
        })
        .flatten()
        .unwrap_or_default();
    if run.tasks == 1 {
        let pass_at_k = if run.pass_at_k_tasks == 1 {
            "pass"
        } else {
            "fail"
        };
        let k = run
            .k
            .map_or_else(|| "pass@k".to_owned(), |k| format!("pass@{k}"));
        return writeln!(
            output,
            "{} · {}{} · {} · thinking {thinking} · trials {}/{} · {k} {pass_at_k} · {trajectory}{errors}{revision}",
            run.submission, run.agent, version, run.model, run.passes, run.trials,
        );
    }
    let pass_at_k = run
        .k
        .map_or_else(|| "task pass".to_owned(), |k| format!("pass@{k}"));
    writeln!(
        output,
        "{} · {}{} · {} · thinking {thinking} · {pass_at_k} {}/{} · trials {}/{} · {trajectory}{errors}",
        run.submission,
        run.agent,
        version,
        run.model,
        run.pass_at_k_tasks,
        run.tasks,
        run.passes,
        run.trials
    )
}

impl ThinkingEvidence {
    fn human(&self) -> String {
        let configured = match self.configured.as_slice() {
            [] => "not published".to_owned(),
            [value] => value.clone(),
            values => format!("mixed [{}]", values.join(", ")),
        };
        match self.provenance {
            ThinkingProvenance::PublishedConfig => {
                if self.configured.is_empty() {
                    configured
                } else {
                    format!("{configured} (published config)")
                }
            }
            ThinkingProvenance::RuntimeEvents => {
                let observed = match self.observed.as_slice() {
                    [] => "none".to_owned(),
                    [value] => value.clone(),
                    values => format!("mixed [{}]", values.join(", ")),
                };
                if self.configured.len() == 1
                    && self.observed == self.configured
                    && self.observed_trials == self.total_trials
                {
                    format!(
                        "{configured} (verified {}/{} runtime events)",
                        self.observed_trials, self.total_trials
                    )
                } else {
                    format!(
                        "configured {configured}, observed {observed} in {}/{} trials",
                        self.observed_trials, self.total_trials
                    )
                }
            }
        }
    }
}

fn write_human(output: &mut impl Write, report: &PublishedTask, full: bool) -> io::Result<()> {
    writeln!(
        output,
        "Published Harbor results for terminal-bench/{}",
        report.task
    )?;
    writeln!(
        output,
        "archive {} · {} results · {} passing",
        &report.archive_revision[..report.archive_revision.len().min(12)],
        report.matching_results,
        report.passing_results
    )?;
    if let Some(checksum) = &report.requested_checksum {
        writeln!(
            output,
            "exact task checksum {checksum}: {} passing",
            report.exact_passing_results
        )?;
    }
    if report.trials.is_empty() {
        writeln!(output, "\nNo matching successful published attempts.")?;
        return Ok(());
    }

    for trial in &report.trials {
        write_trial(output, report, trial, full)?;
    }
    Ok(())
}

fn write_trial(
    output: &mut impl Write,
    report: &PublishedTask,
    trial: &PublishedTrial,
    full: bool,
) -> io::Result<()> {
    let exact = report
        .requested_checksum
        .as_deref()
        .is_some_and(|checksum| checksum == trial.task_checksum);
    let revision = if report.requested_checksum.is_none() || exact {
        "exact"
    } else {
        "different revision"
    };
    let model = trial
        .agent
        .model_info
        .as_ref()
        .map_or("unknown model", |model| model.name.as_str());
    writeln!(
        output,
        "\n{} · {} · {} · reward {:.3} · {}",
        trial.submission, trial.agent.name, model, trial.reward, revision
    )?;
    writeln!(
        output,
        "trial {} · checksum {}",
        trial.trial_name, trial.task_checksum
    )?;
    let Some(trajectory) = &trial.trajectory else {
        match &trial.trajectory_error {
            Some(error) => writeln!(output, "published trajectory could not be decoded: {error}")?,
            None => writeln!(
                output,
                "trajectory unavailable in publication; pass result is cached"
            )?,
        }
        return Ok(());
    };
    writeln!(
        output,
        "{} · {} steps",
        trajectory.schema_version,
        trajectory.steps.len()
    )?;
    for step in &trajectory.steps {
        if full && let Some(reasoning) = nonempty(step.reasoning_content.as_deref()) {
            writeln!(output, "  reasoning: {reasoning}")?;
        }
        if let Some(message) = nonempty(step.message.as_deref())
            && (full || step.source == "user" || step.tool_calls.is_empty())
        {
            writeln!(output, "  {}: {}", step.source, one_line(message, full))?;
        }
        for call in &step.tool_calls {
            writeln!(
                output,
                "  tool {} {}",
                call.function_name,
                one_line(call.arguments.get(), full)
            )?;
        }
        if full && let Some(observation) = &step.observation {
            for result in &observation.results {
                writeln!(
                    output,
                    "  result {}: {}",
                    result.source_call_id, result.content
                )?;
            }
        }
    }
    Ok(())
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.trim().is_empty())
}

fn one_line(value: &str, full: bool) -> String {
    const LIMIT: usize = 240;

    if full {
        return value.to_owned();
    }
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut characters = compact.chars();
    let prefix = characters.by_ref().take(LIMIT).collect::<String>();
    if characters.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use nanoeval_harbor::{PublishedAttempt, PublishedModelInfo};

    use super::*;

    #[test]
    fn compact_output_flattens_and_bounds_content() {
        let input = format!("first\n{}\nlast", "x".repeat(300));
        let output = one_line(&input, false);
        assert!(!output.contains('\n'));
        assert!(output.ends_with('…'));
        assert!(output.chars().count() <= 241);
    }

    #[test]
    fn job_comparison_ranks_only_complete_exact_runs() {
        let local = LocalJob {
            agent: "nanocodex".to_owned(),
            agent_version: Some("0.1.0".to_owned()),
            model: "gpt".to_owned(),
            thinking_configured: BTreeSet::from(["high".to_owned()]),
            thinking_observed: BTreeSet::from(["high".to_owned()]),
            thinking_observed_trials: 4,
            tasks: BTreeMap::from([
                (
                    "terminal-bench/first".to_owned(),
                    TaskScore {
                        task_name: "terminal-bench/first".to_owned(),
                        task_checksum: "first-checksum".to_owned(),
                        trials: 2,
                        passes: 1,
                        errors: 0,
                        trajectories: 2,
                    },
                ),
                (
                    "terminal-bench/second".to_owned(),
                    TaskScore {
                        task_name: "terminal-bench/second".to_owned(),
                        task_checksum: "second-checksum".to_owned(),
                        trials: 2,
                        passes: 0,
                        errors: 0,
                        trajectories: 2,
                    },
                ),
            ]),
            trials: BTreeMap::new(),
        };
        let agent = PublishedAgentInfo {
            name: "other".to_owned(),
            version: None,
            model_info: Some(PublishedModelInfo {
                name: "model".to_owned(),
                provider: None,
            }),
        };
        let task = |name: &str, checksum: &str, passed: bool| PublishedAttempts {
            task: name.to_owned(),
            requested_checksum: Some(checksum.to_owned()),
            archive_revision: "revision".to_owned(),
            attempts: vec![PublishedAttempt {
                submission: "submission".to_owned(),
                run: "run".to_owned(),
                task_name: name.to_owned(),
                task_checksum: checksum.to_owned(),
                trial_name: format!("{name}-trial"),
                passed,
                errored: false,
                agent: agent.clone(),
                thinking: Some("medium".to_owned()),
                agent_import_path: Some("other.agent:Agent".to_owned()),
                result_path: format!("{name}/result.json"),
                trajectory_path: Some(format!("{name}/trajectory.json")),
            }],
        };

        let report = JobComparison::new(
            PathBuf::from("job"),
            local,
            vec![
                task("terminal-bench/first", "first-checksum", true),
                task("terminal-bench/second", "second-checksum", false),
            ],
            10,
            0.1,
            None,
        )
        .unwrap();

        assert_eq!(report.local.pass_at_k_tasks, 1);
        assert_eq!(report.exact.len(), 1);
        assert_eq!(report.exact[0].pass_at_k_tasks, 1);
        assert_eq!(report.exact[0].tasks, 2);
    }
}

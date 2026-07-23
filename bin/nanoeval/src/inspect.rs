use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use clap::Args;
use eyre::{Result, eyre};
use nanoeval::{AtifSource, AtifTrajectory};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use uuid::Uuid;
use yansi::Painted;

#[derive(Args)]
pub(crate) struct Inspect {
    /// Harbor job or trial directory to inspect.
    #[arg(value_name = "DIRECTORY")]
    directory: PathBuf,

    /// Select one trial by its exact name or unique prefix.
    #[arg(long, value_name = "NAME")]
    trial: Option<String>,

    /// Emit the typed inspection report as JSON.
    #[arg(long)]
    json: bool,

    /// Include complete verifier, agent stderr, and VM network logs.
    #[arg(long)]
    full: bool,
}

impl Inspect {
    pub(crate) fn run(self) -> Result<()> {
        let directory = self.directory.canonicalize()?;
        let report = if directory.join("agent/trajectory.json").is_file() {
            if self.trial.is_some() {
                return Err(eyre!("--trial is only valid when inspecting a job"));
            }
            Inspection::Trial(Box::new(TrialInspection::load(&directory, self.full)?))
        } else {
            let job = JobInspection::load(&directory, self.full)?;
            match self.trial {
                Some(selector) => {
                    Inspection::Trial(Box::new(job.select_trial(&selector)?.to_owned()))
                }
                None => Inspection::Job(job),
            }
        };
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
}

#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Inspection {
    Job(JobInspection),
    Trial(Box<TrialInspection>),
}

impl Inspection {
    fn write_human(&self, output: &mut impl Write, full: bool) -> io::Result<()> {
        match self {
            Self::Job(job) => job.write_human(output),
            Self::Trial(trial) => trial.write_human(output, full),
        }
    }
}

#[derive(Clone, Serialize)]
struct JobInspection {
    id: Uuid,
    directory: PathBuf,
    total: usize,
    passed: usize,
    failed: usize,
    refused: usize,
    errored: usize,
    trials: Vec<TrialInspection>,
}

impl JobInspection {
    fn load(directory: &Path, full: bool) -> Result<Self> {
        let result = read_json::<HarborJobResult>(&directory.join("result.json"))?;
        let mut trial_directories = fs::read_dir(directory)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir() && path.join("agent/trajectory.json").is_file())
            .collect::<Vec<_>>();
        trial_directories.sort();
        let trials = trial_directories
            .iter()
            .map(|path| TrialInspection::load(path, full))
            .collect::<Result<Vec<_>>>()?;
        let passed = trials
            .iter()
            .filter(|trial| trial.status == TrialStatus::Passed)
            .count();
        let failed = trials
            .iter()
            .filter(|trial| trial.status == TrialStatus::Failed)
            .count();
        let refused = trials
            .iter()
            .filter(|trial| trial.status == TrialStatus::Refused)
            .count();
        let errored = trials
            .iter()
            .filter(|trial| trial.status == TrialStatus::Errored)
            .count();
        Ok(Self {
            id: result.id,
            directory: directory.to_path_buf(),
            total: result.n_total_trials,
            passed,
            failed,
            refused,
            errored,
            trials,
        })
    }

    fn select_trial(&self, selector: &str) -> Result<&TrialInspection> {
        if let Some(trial) = self
            .trials
            .iter()
            .find(|trial| trial.trial_name == selector)
        {
            return Ok(trial);
        }
        let matches = self
            .trials
            .iter()
            .filter(|trial| trial.trial_name.starts_with(selector))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [trial] => Ok(*trial),
            [] => Err(eyre!("job contains no trial matching {selector:?}")),
            _ => Err(eyre!(
                "trial selector {selector:?} is ambiguous; use the exact trial name"
            )),
        }
    }

    fn write_human(&self, output: &mut impl Write) -> io::Result<()> {
        writeln!(
            output,
            "Job {}: {} passed, {} failed, {} refused, {} errored ({} retained / {} expected)",
            self.id,
            self.passed,
            self.failed,
            self.refused,
            self.errored,
            self.trials.len(),
            self.total
        )?;
        writeln!(output, "{}", self.directory.display())?;
        for trial in &self.trials {
            trial.write_summary(output)?;
            if trial.status != TrialStatus::Passed {
                trial.write_failure_summary(output)?;
            }
        }
        if self.failed + self.refused + self.errored > 0 {
            writeln!(
                output,
                "\nUse `nanoeval inspect {} --trial <name> --full` for complete evidence.",
                self.directory.display()
            )?;
        }
        Ok(())
    }
}

#[derive(Clone, Serialize)]
struct TrialInspection {
    id: Uuid,
    task_name: String,
    trial_name: String,
    status: TrialStatus,
    reward: Option<f64>,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    duration: DurationMillis,
    phases: PhaseInspection,
    agent: Option<AgentInspection>,
    exception: Option<ExceptionInspection>,
    tests: Option<TestInspection>,
    final_response: Option<String>,
    artifacts: ArtifactPaths,
    full_output: Option<FullOutput>,
}

impl TrialInspection {
    fn load(directory: &Path, full: bool) -> Result<Self> {
        let result = read_json::<HarborTrialResult>(&directory.join("result.json"))?;
        let trajectory = read_json::<AtifTrajectory>(&directory.join("agent/trajectory.json"))?;
        let ctrf = read_optional_json::<CtrfReport>(&directory.join("verifier/ctrf.json"))?;
        let reward = result
            .verifier_result
            .as_ref()
            .and_then(|verifier| verifier.rewards.get("reward"))
            .copied();
        let status = trial_status(
            result
                .exception_info
                .as_ref()
                .map(|exception| exception.exception_type.as_str()),
            reward,
        );
        let artifacts = ArtifactPaths::new(directory);
        let full_output = full.then(|| FullOutput::load(&artifacts)).transpose()?;
        let phases = PhaseInspection::from_result(&result);
        Ok(Self {
            id: result.id,
            task_name: result.task_name,
            trial_name: result.trial_name,
            status,
            reward,
            started_at: result.started_at,
            finished_at: result.finished_at,
            duration: DurationMillis(
                result
                    .finished_at
                    .signed_duration_since(result.started_at)
                    .num_milliseconds(),
            ),
            phases,
            agent: result.agent_result.map(Into::into),
            exception: result.exception_info.map(Into::into),
            tests: ctrf.map(Into::into),
            final_response: trajectory
                .steps
                .iter()
                .rev()
                .find(|step| matches!(step.source, AtifSource::Agent) && !step.message.is_empty())
                .map(|step| step.message.clone()),
            artifacts,
            full_output,
        })
    }

    fn write_human(&self, output: &mut impl Write, full: bool) -> io::Result<()> {
        self.write_summary(output)?;
        writeln!(output, "task: {}", self.task_name)?;
        writeln!(
            output,
            "duration: {} (agent {}, verifier {})",
            self.duration,
            format_duration(self.phases.agent_execution),
            format_duration(self.phases.verifier)
        )?;
        if let Some(agent) = &self.agent {
            writeln!(
                output,
                "agent: {} model calls, {} tool calls, {} input / {} cached / {} output tokens ({}.{:01}% cache)",
                agent.model_calls,
                agent.tool_calls,
                agent.input_tokens,
                agent.cached_tokens,
                agent.output_tokens,
                agent.cache_percent_tenths / 10,
                agent.cache_percent_tenths % 10
            )?;
        }
        self.write_failure_reason(output)?;
        if let Some(response) = &self.final_response {
            writeln!(output, "\nFinal agent response:\n{response}")?;
        }
        writeln!(output, "\nArtifacts:")?;
        self.artifacts.write_human(output)?;
        if full && let Some(full_output) = &self.full_output {
            full_output.write_human(output)?;
        }
        Ok(())
    }

    fn write_summary(&self, output: &mut impl Write) -> io::Result<()> {
        let reward = self
            .reward
            .map_or_else(|| "-".to_owned(), |reward| format!("{reward:.3}"));
        writeln!(
            output,
            "{} {} reward={reward}",
            self.status.label(),
            self.trial_name
        )
    }

    fn write_failure_reason(&self, output: &mut impl Write) -> io::Result<()> {
        if let Some(exception) = &self.exception {
            writeln!(
                output,
                "  exception: {}: {}",
                exception.exception_type, exception.message
            )?;
        }
        if let Some(tests) = &self.tests {
            writeln!(
                output,
                "  tests: {} passed, {} failed, {} skipped",
                tests.passed, tests.failed, tests.skipped
            )?;
            for test in &tests.failures {
                writeln!(output, "  - {} [{}]", test.name, test.status.as_str())?;
                if let Some(message) = &test.message {
                    writeln!(output, "    {message}")?;
                }
                if let Some(trace) = &test.trace {
                    for line in trace.lines() {
                        writeln!(output, "    {line}")?;
                    }
                }
            }
        }
        Ok(())
    }

    fn write_failure_summary(&self, output: &mut impl Write) -> io::Result<()> {
        if let Some(exception) = &self.exception {
            writeln!(
                output,
                "  exception: {}: {}",
                exception.exception_type, exception.message
            )?;
        }
        if let Some(tests) = &self.tests {
            writeln!(
                output,
                "  tests: {} passed, {} failed, {} skipped",
                tests.passed, tests.failed, tests.skipped
            )?;
            for test in &tests.failures {
                let message = test
                    .message
                    .as_deref()
                    .and_then(|message| message.lines().next())
                    .unwrap_or("no failure message");
                writeln!(
                    output,
                    "  - {} [{}]: {message}",
                    test.name,
                    test.status.as_str()
                )?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum TrialStatus {
    Passed,
    Failed,
    Refused,
    Errored,
}

impl TrialStatus {
    fn label(self) -> Painted<&'static str> {
        match self {
            Self::Passed => Painted::new("PASS   ").green(),
            Self::Failed => Painted::new("FAIL   ").red(),
            Self::Refused => Painted::new("REFUSED").yellow(),
            Self::Errored => Painted::new("ERROR  ").red(),
        }
    }
}

fn trial_status(exception_type: Option<&str>, reward: Option<f64>) -> TrialStatus {
    match (exception_type, reward) {
        (Some("AgentSafetyRefusalError"), _) => TrialStatus::Refused,
        (Some(_), _) => TrialStatus::Errored,
        (None, Some(1.0)) => TrialStatus::Passed,
        (None, _) => TrialStatus::Failed,
    }
}

#[derive(Clone, Serialize)]
struct PhaseInspection {
    environment_setup: Option<DurationMillis>,
    agent_setup: Option<DurationMillis>,
    agent_execution: Option<DurationMillis>,
    verifier: Option<DurationMillis>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(transparent)]
struct DurationMillis(i64);

impl std::fmt::Display for DurationMillis {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let seconds = self.0 / 1_000;
        let millis = self.0.unsigned_abs() % 1_000;
        write!(formatter, "{seconds}.{millis:03}s")
    }
}

impl PhaseInspection {
    fn from_result(result: &HarborTrialResult) -> Self {
        Self {
            environment_setup: phase_duration(result.environment_setup.as_ref()),
            agent_setup: phase_duration(result.agent_setup.as_ref()),
            agent_execution: phase_duration(result.agent_execution.as_ref()),
            verifier: phase_duration(result.verifier.as_ref()),
        }
    }
}

#[derive(Clone, Serialize)]
struct AgentInspection {
    input_tokens: u64,
    cached_tokens: u64,
    output_tokens: u64,
    cache_percent_tenths: u16,
    model_calls: u32,
    tool_calls: u32,
}

impl From<HarborAgentResult> for AgentInspection {
    fn from(result: HarborAgentResult) -> Self {
        let cache_percent_tenths = if result.n_input_tokens == 0 {
            0
        } else {
            u16::try_from(
                u128::from(result.n_cache_tokens).saturating_mul(1_000)
                    / u128::from(result.n_input_tokens),
            )
            .unwrap_or(u16::MAX)
        };
        Self {
            input_tokens: result.n_input_tokens,
            cached_tokens: result.n_cache_tokens,
            output_tokens: result.n_output_tokens,
            cache_percent_tenths,
            model_calls: result.metadata.model_calls,
            tool_calls: result.metadata.tool_calls,
        }
    }
}

#[derive(Clone, Serialize)]
struct ExceptionInspection {
    exception_type: String,
    message: String,
    traceback: String,
    occurred_at: DateTime<Utc>,
}

impl From<HarborExceptionInfo> for ExceptionInspection {
    fn from(exception: HarborExceptionInfo) -> Self {
        Self {
            exception_type: exception.exception_type,
            message: exception.exception_message,
            traceback: exception.exception_traceback,
            occurred_at: exception.occurred_at,
        }
    }
}

#[derive(Clone, Serialize)]
struct TestInspection {
    total: u32,
    passed: u32,
    failed: u32,
    skipped: u32,
    failures: Vec<TestFailure>,
}

impl From<CtrfReport> for TestInspection {
    fn from(report: CtrfReport) -> Self {
        let failures = report
            .results
            .tests
            .into_iter()
            .filter(|test| test.status != CtrfStatus::Passed)
            .map(Into::into)
            .collect();
        Self {
            total: report.results.summary.tests,
            passed: report.results.summary.passed,
            failed: report.results.summary.failed,
            skipped: report.results.summary.skipped,
            failures,
        }
    }
}

#[derive(Clone, Serialize)]
struct TestFailure {
    name: String,
    status: CtrfStatus,
    duration_seconds: Option<f64>,
    message: Option<String>,
    trace: Option<String>,
}

impl From<CtrfTest> for TestFailure {
    fn from(test: CtrfTest) -> Self {
        Self {
            name: test.name,
            status: test.status,
            duration_seconds: test.duration,
            message: test.message,
            trace: test.trace,
        }
    }
}

#[derive(Clone, Serialize)]
struct ArtifactPaths {
    result: PathBuf,
    trajectory: PathBuf,
    events: PathBuf,
    verifier_output: PathBuf,
    ctrf: PathBuf,
    agent_stderr: PathBuf,
    network_log: PathBuf,
    rootfs: PathBuf,
}

impl ArtifactPaths {
    fn new(directory: &Path) -> Self {
        Self {
            result: directory.join("result.json"),
            trajectory: directory.join("agent/trajectory.json"),
            events: directory.join("agent/events.jsonl"),
            verifier_output: directory.join("verifier/test-stdout.txt"),
            ctrf: directory.join("verifier/ctrf.json"),
            agent_stderr: directory.join("agent/stderr.log"),
            network_log: directory.join("vm/gvproxy.log"),
            rootfs: directory.join("rootfs.ext4"),
        }
    }

    fn write_human(&self, output: &mut impl Write) -> io::Result<()> {
        for (name, path) in [
            ("result", &self.result),
            ("trajectory", &self.trajectory),
            ("events", &self.events),
            ("verifier", &self.verifier_output),
            ("ctrf", &self.ctrf),
            ("agent stderr", &self.agent_stderr),
            ("VM network", &self.network_log),
            ("retained rootfs", &self.rootfs),
        ] {
            if path.exists() {
                writeln!(output, "  {name}: {}", path.display())?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Serialize)]
struct FullOutput {
    verifier: Option<String>,
    agent_stderr: Option<String>,
    network: Option<String>,
}

impl FullOutput {
    fn load(paths: &ArtifactPaths) -> io::Result<Self> {
        Ok(Self {
            verifier: read_optional_text(&paths.verifier_output)?,
            agent_stderr: read_optional_text(&paths.agent_stderr)?,
            network: read_optional_text(&paths.network_log)?,
        })
    }

    fn write_human(&self, output: &mut impl Write) -> io::Result<()> {
        for (name, contents) in [
            ("Verifier output", self.verifier.as_deref()),
            ("Agent stderr", self.agent_stderr.as_deref()),
            ("VM network log", self.network.as_deref()),
        ] {
            if let Some(contents) = contents.filter(|contents| !contents.is_empty()) {
                writeln!(output, "\n{name}:\n{contents}")?;
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct HarborJobResult {
    id: Uuid,
    n_total_trials: usize,
}

#[derive(Deserialize)]
struct HarborTrialResult {
    id: Uuid,
    task_name: String,
    trial_name: String,
    agent_result: Option<HarborAgentResult>,
    verifier_result: Option<HarborVerifierResult>,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    environment_setup: Option<HarborPhaseTiming>,
    agent_setup: Option<HarborPhaseTiming>,
    agent_execution: Option<HarborPhaseTiming>,
    verifier: Option<HarborPhaseTiming>,
    exception_info: Option<HarborExceptionInfo>,
}

#[derive(Deserialize)]
struct HarborAgentResult {
    n_input_tokens: u64,
    n_cache_tokens: u64,
    n_output_tokens: u64,
    metadata: nanoeval::AgentMetadata,
}

#[derive(Deserialize)]
struct HarborVerifierResult {
    rewards: BTreeMap<String, f64>,
}

#[derive(Deserialize)]
struct HarborExceptionInfo {
    exception_type: String,
    exception_message: String,
    exception_traceback: String,
    occurred_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct CtrfReport {
    results: CtrfResults,
}

#[derive(Deserialize)]
struct CtrfResults {
    summary: CtrfSummary,
    tests: Vec<CtrfTest>,
}

#[derive(Deserialize)]
struct CtrfSummary {
    tests: u32,
    passed: u32,
    failed: u32,
    skipped: u32,
}

#[derive(Deserialize)]
struct CtrfTest {
    name: String,
    status: CtrfStatus,
    duration: Option<f64>,
    message: Option<String>,
    trace: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum CtrfStatus {
    Passed,
    Failed,
    Skipped,
    Pending,
    Other,
}

impl CtrfStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::Pending => "pending",
            Self::Other => "other",
        }
    }
}

#[derive(Deserialize)]
struct HarborPhaseTiming {
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
}

fn phase_duration(timing: Option<&HarborPhaseTiming>) -> Option<DurationMillis> {
    timing.map(|timing| {
        DurationMillis(
            timing
                .finished_at
                .signed_duration_since(timing.started_at)
                .num_milliseconds(),
        )
    })
}

fn format_duration(duration: Option<DurationMillis>) -> String {
    duration.map_or_else(|| "-".to_owned(), |duration| duration.to_string())
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let contents = fs::read(path)?;
    serde_json::from_slice(&contents).map_err(Into::into)
}

fn read_optional_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(contents) => serde_json::from_slice(&contents)
            .map(Some)
            .map_err(Into::into),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_optional_text(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::{TrialStatus, trial_status};

    #[test]
    fn classifies_refusals_separately_from_errors() {
        assert_eq!(
            trial_status(Some("AgentSafetyRefusalError"), None),
            TrialStatus::Refused
        );
        assert_eq!(
            trial_status(Some("AgentAuthenticationError"), None),
            TrialStatus::Errored
        );
    }

    #[test]
    fn classifies_scored_trials_from_reward() {
        assert_eq!(trial_status(None, Some(1.0)), TrialStatus::Passed);
        assert_eq!(trial_status(None, Some(0.0)), TrialStatus::Failed);
        assert_eq!(trial_status(None, None), TrialStatus::Failed);
    }
}

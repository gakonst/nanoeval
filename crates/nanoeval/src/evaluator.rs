use std::{
    io::{BufWriter, Write},
    num::ParseFloatError,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::{DateTime, Utc};
use futures_util::{StreamExt, stream};
use nanocodex::{AgentEvent, AgentEventKind, NanocodexBuilder, NanocodexError, StandardResponses};
use serde::{Deserialize, Serialize};
use tokio::{sync::AcquireError, sync::Semaphore, time::timeout};
use uuid::Uuid;

use crate::{
    AgentResult, EvalArtifacts, EvalEvent, EvalEventKind, EvalResult, EvalStatus, EvalTiming,
    NanoevalEvents, PhaseTiming, Task,
    harbor::{HarborArtifacts, HarborEventSummary},
    native::NativeAttempt,
};

/// A reusable evaluation recipe. Every task call creates an independent agent
/// session and disposable workspace.
#[derive(Clone)]
pub struct Nanoeval {
    inner: Arc<NanoevalInner>,
}

/// Deliberate evaluator policy configured before running tasks.
pub struct NanoevalBuilder {
    nanocodex: NanocodexBuilder<StandardResponses>,
    output_directory: PathBuf,
    max_concurrency: usize,
}

struct NanoevalInner {
    nanocodex: NanocodexBuilder<StandardResponses>,
    harbor: HarborArtifacts,
    concurrency: Arc<Semaphore>,
    max_concurrency: usize,
    events: tokio::sync::mpsc::UnboundedSender<EvalEvent>,
    results: Mutex<Vec<EvalResult>>,
}

#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    #[error("maximum concurrency must be greater than zero")]
    InvalidConcurrency,

    #[error("task {task} cannot run with the native backend: {reason}")]
    UnsupportedNativeTask { task: String, reason: &'static str },

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("unsupported non-file entry in task environment: {0}")]
    UnsupportedEnvironmentEntry(PathBuf),

    #[error("trial directory cannot be represented as a file URL: {0}")]
    InvalidTrialPath(PathBuf),

    #[error("task directory is empty: {0}")]
    EmptyTask(PathBuf),

    #[error("task directory contains a cyclic symbolic link: {0}")]
    CyclicTaskDirectory(PathBuf),

    #[error("failed to compile task ignore rules: {0}")]
    Ignore(#[from] ignore::Error),

    #[error("Nanocodex failed: {0}")]
    Nanocodex(#[from] NanocodexError),

    #[error("agent exceeded its {0:?} timeout")]
    AgentTimeout(Duration),

    #[error("verifier exceeded its {0:?} timeout")]
    VerifierTimeout(Duration),

    #[error("agent event stream closed before a terminal event")]
    AgentEventsClosed,

    #[error("failed to encode or decode JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("invalid verifier reward: {0}")]
    ParseReward(#[from] ParseFloatError),

    #[error("Nanoeval's concurrency semaphore was unexpectedly closed")]
    ConcurrencyClosed(#[from] AcquireError),
}

impl Nanoeval {
    #[must_use]
    pub fn builder(nanocodex: NanocodexBuilder<StandardResponses>) -> NanoevalBuilder {
        NanoevalBuilder {
            nanocodex,
            output_directory: PathBuf::from("nanoeval-runs"),
            max_concurrency: 1,
        }
    }

    /// Runs one independent attempt.
    ///
    /// # Errors
    ///
    /// Returns an error when setup, the agent, verification, or artifact
    /// publication fails.
    pub async fn task(&self, task: Task) -> Result<EvalResult, EvalError> {
        let _permit = Arc::clone(&self.inner.concurrency).acquire_owned().await?;
        self.run_task(task).await
    }

    /// Runs `count` fresh attempts of the same immutable task.
    ///
    /// Results preserve attempt order even when work completes out of order.
    ///
    /// # Errors
    ///
    /// Returns the first setup, agent, verifier, or publication error.
    pub async fn task_n(&self, task: Task, count: usize) -> Result<Vec<EvalResult>, EvalError> {
        self.tasks(std::iter::repeat_n(task, count)).await
    }

    /// Runs independent tasks with the configured concurrency bound.
    ///
    /// # Errors
    ///
    /// Returns the first setup, agent, verifier, or publication error.
    pub async fn tasks(
        &self,
        tasks: impl IntoIterator<Item = Task>,
    ) -> Result<Vec<EvalResult>, EvalError> {
        let evaluator = self.clone();
        let mut completed = stream::iter(tasks.into_iter().enumerate())
            .map(move |(index, task)| {
                let evaluator = evaluator.clone();
                async move { (index, evaluator.task(task).await) }
            })
            .buffer_unordered(self.inner.max_concurrency);
        let mut results = Vec::new();
        while let Some((index, result)) = completed.next().await {
            results.push((index, result?));
        }
        results.sort_unstable_by_key(|(index, _)| *index);
        Ok(results.into_iter().map(|(_, result)| result).collect())
    }

    #[must_use]
    pub fn output_directory(&self) -> &Path {
        self.inner.harbor.root()
    }

    async fn run_task(&self, task: Task) -> Result<EvalResult, EvalError> {
        let started_at = Utc::now();
        let attempt_id = Uuid::new_v4();
        let trial_name = HarborArtifacts::trial_name(&task, attempt_id);
        let attempt = NativeAttempt::prepare(self.inner.harbor.root(), &trial_name, &task)?;
        HarborArtifacts::write_input(&attempt, &task)?;
        let agent = self.execute_agent(attempt_id, &task, &attempt).await?;

        self.emit(attempt_id, task.name(), EvalEventKind::VerifierStarted);
        let verifier = attempt.verify(&task).await?;
        self.emit(
            attempt_id,
            task.name(),
            EvalEventKind::VerifierOutput {
                stdout: verifier.stdout.clone(),
                stderr: verifier.stderr.clone(),
            },
        );
        self.emit(
            attempt_id,
            task.name(),
            EvalEventKind::VerifierCompleted(verifier.result.clone()),
        );

        let result = EvalResult {
            attempt_id,
            task_name: task.name().to_owned(),
            trial_name,
            status: if verifier.result.rewards.values().all(|reward| *reward > 0.0) {
                EvalStatus::Passed
            } else {
                EvalStatus::Failed
            },
            agent: agent.result,
            verifier: verifier.result,
            timing: EvalTiming {
                started_at,
                finished_at: Utc::now(),
                environment_setup: attempt.setup_timing.clone(),
                agent_setup: agent.setup_timing,
                agent_execution: agent.execution_timing,
                verifier: verifier.timing,
            },
            artifacts: EvalArtifacts {
                directory: attempt.paths.root.clone(),
                workspace: attempt.paths.workspace.clone(),
                events_jsonl: attempt.paths.events.clone(),
                trajectory_json: attempt.paths.trajectory.clone(),
                verifier_output: attempt.paths.verifier_output.clone(),
                result_json: attempt.paths.result.clone(),
            },
        };
        self.inner
            .harbor
            .write_trial(&attempt, &task, &result, &agent.event_summary)?;
        {
            let mut results = self
                .inner
                .results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            results.push(result.clone());
            self.inner.harbor.write_job(&results)?;
        }
        self.emit(
            attempt_id,
            task.name(),
            EvalEventKind::Completed(Box::new(result.clone())),
        );
        Ok(result)
    }

    async fn execute_agent(
        &self,
        attempt_id: Uuid,
        task: &Task,
        attempt: &NativeAttempt,
    ) -> Result<AgentExecution, EvalError> {
        let setup_started = Utc::now();
        let (agent, mut events) = self
            .inner
            .nanocodex
            .clone()
            .workspace(&attempt.paths.workspace)
            .session_id(attempt_id.to_string())
            .build()?;
        let setup_timing = PhaseTiming::finished(setup_started);
        let execution_started = Utc::now();
        let turn = agent.prompt(task.prompt()).await?;
        let control = turn.control();
        let mut event_summary = HarborEventSummary::default();
        let event_result = timeout(task.agent_timeout(), async {
            let file = std::fs::File::create(&attempt.paths.events)?;
            let mut writer = BufWriter::new(file);
            loop {
                let event = events.recv().await.ok_or(EvalError::AgentEventsClosed)?;
                serde_json::to_writer(&mut writer, &event)?;
                writer.write_all(b"\n")?;
                writer.flush()?;
                let terminal = event.kind.is_terminal();
                event_summary.observe(&event)?;
                self.emit(attempt_id, task.name(), EvalEventKind::Agent(event.clone()));
                if terminal {
                    let result = turn.result().await?;
                    return Ok::<_, EvalError>((result, event));
                }
            }
        })
        .await;
        let (turn_result, terminal_event) = if let Ok(result) = event_result {
            result?
        } else {
            let _ = control.cancel().await;
            return Err(EvalError::AgentTimeout(task.agent_timeout()));
        };
        drop(agent);
        Ok(AgentExecution {
            result: AgentResult::from_terminal(turn_result.final_message, &terminal_event)?,
            event_summary,
            setup_timing,
            execution_timing: PhaseTiming::finished(execution_started),
        })
    }

    fn emit(&self, attempt_id: Uuid, task_name: &str, kind: EvalEventKind) {
        let _ = self.inner.events.send(EvalEvent {
            attempt_id,
            task_name: task_name.to_owned(),
            kind,
        });
    }
}

struct AgentExecution {
    result: AgentResult,
    event_summary: HarborEventSummary,
    setup_timing: PhaseTiming,
    execution_timing: PhaseTiming,
}

impl NanoevalBuilder {
    #[must_use]
    pub fn output_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.output_directory = directory.into();
        self
    }

    #[must_use]
    pub const fn max_concurrency(mut self, max_concurrency: usize) -> Self {
        self.max_concurrency = max_concurrency;
        self
    }

    /// Builds a reusable evaluator and its optional multiplexed event stream.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid concurrency or an unavailable output path.
    pub fn build(self) -> Result<(Nanoeval, NanoevalEvents), EvalError> {
        if self.max_concurrency == 0 {
            return Err(EvalError::InvalidConcurrency);
        }
        let harbor = HarborArtifacts::create(&self.output_directory, self.max_concurrency)?;
        let (event_sender, event_receiver) = tokio::sync::mpsc::unbounded_channel();
        Ok((
            Nanoeval {
                inner: Arc::new(NanoevalInner {
                    nanocodex: self.nanocodex,
                    harbor,
                    concurrency: Arc::new(Semaphore::new(self.max_concurrency)),
                    max_concurrency: self.max_concurrency,
                    events: event_sender,
                    results: Mutex::new(Vec::new()),
                }),
            },
            NanoevalEvents::new(event_receiver),
        ))
    }
}

impl AgentResult {
    fn from_terminal(final_message: String, event: &AgentEvent) -> Result<Self, EvalError> {
        if event.kind != AgentEventKind::RunCompleted {
            return Err(EvalError::AgentEventsClosed);
        }
        let terminal: NanocodexTerminalPayload = serde_json::from_str(event.payload.get())?;
        let metadata = serde_json::to_value(&terminal)?;
        Ok(Self {
            final_message,
            model: terminal.model,
            effort: terminal.effort,
            model_calls: terminal.model_calls,
            tool_calls: terminal.tool_calls,
            usage: terminal.usage,
            cost_usd: terminal.cost_usd,
            metadata,
        })
    }
}

impl PhaseTiming {
    fn finished(started_at: DateTime<Utc>) -> Self {
        Self {
            started_at,
            finished_at: Utc::now(),
        }
    }
}

#[derive(Deserialize, Serialize)]
struct NanocodexTerminalPayload {
    status: TerminalStatus,
    model: String,
    effort: String,
    transport: String,
    orchestration: String,
    duration_ms: u64,
    duration_ns: u64,
    model_calls: u32,
    steers: u32,
    compactions: u32,
    tool_calls: u32,
    connection_attempts: u32,
    websocket_reconnects: u32,
    response_attempts: u32,
    response_retries: u32,
    connection_duration_ns: u64,
    retry_backoff_duration_ns: u64,
    model_duration_ns: u64,
    warmup_duration_ns: u64,
    tool_work_duration_ns: u64,
    tool_wall_duration_ns: u64,
    usage: crate::UsageTotals,
    warmup_usage: crate::UsageTotals,
    #[serde(rename = "last_response_id", skip_serializing)]
    _last_response_id: Option<String>,
    cost_usd: Option<f64>,
    cost_status: String,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum TerminalStatus {
    Completed,
    Failed,
    Cancelled,
}

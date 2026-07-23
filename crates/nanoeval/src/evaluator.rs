use std::{
    error::Error,
    future::Future,
    num::ParseFloatError,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use futures_util::{StreamExt, stream};
use nanocodex::{
    AgentEvent, AgentEventKind, MODEL, NanocodexBuilder, NanocodexError, ResponsesError,
    StandardResponses,
};
use serde::Deserialize;
use tokio::{
    sync::{AcquireError, Semaphore, broadcast},
    time::timeout,
};
use tracing::{Instrument, Span, info, info_span};
use uuid::Uuid;

use crate::{
    AgentId, AgentMetadata, AgentResult, EvalArtifacts, EvalEvent, EvalEventKind, EvalFailure,
    EvalFailureKind, EvalResult, EvalStatus, EvalTiming, NanoevalEvents, PhaseTiming, Sweep,
    SweepAttemptResult, SweepResults, Task, VerifierResult,
    job::EvalJob,
    native::{NativeAttempt, VerifierExecution},
};

const EVENT_CAPACITY: usize = 16_384;
// One warmup plus three typical four-call attempts stays below the provider's
// approximate 15-request-per-minute routing guidance for a cache key.
const PROMPT_CACHE_COHORT_SIZE: u64 = 3;

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
    attempt_agent: Option<AttemptAgentFactory>,
}

struct NanoevalInner {
    nanocodex: NanocodexBuilder<StandardResponses>,
    job: EvalJob,
    concurrency: Arc<Semaphore>,
    max_concurrency: usize,
    next_prompt_cache_attempt: AtomicU64,
    events: broadcast::Sender<Arc<EvalEvent>>,
    attempt_agent: Option<AttemptAgentFactory>,
}

type AttemptError = Box<dyn Error + Send + Sync + 'static>;
type AttemptAgentFactory = Arc<
    dyn for<'a> Fn(
            EvalAttempt<'a>,
            NanocodexBuilder<StandardResponses>,
        ) -> Result<AttemptAgent, AttemptError>
        + Send
        + Sync
        + 'static,
>;

type AttemptVerifierFuture<'a> =
    Pin<Box<dyn Future<Output = Result<AttemptVerification, AttemptError>> + Send + 'a>>;

/// The Nanocodex configuration and resources owned by one attempt.
pub struct AttemptAgent {
    nanocodex: NanocodexBuilder<StandardResponses>,
    verifier: Option<Box<dyn AttemptVerifier>>,
}

/// A verifier that runs against the same retained environment as the agent.
pub trait AttemptVerifier: Send {
    fn verify<'a>(
        &'a mut self,
        task: &'a Task,
        attempt: EvalAttempt<'a>,
    ) -> AttemptVerifierFuture<'a>;
}

/// Complete typed output returned by an attempt-owned verifier.
pub struct AttemptVerification {
    pub result: VerifierResult,
    pub stdout: String,
    pub stderr: String,
}

struct AttemptInput {
    task: Task,
    nanocodex: NanocodexBuilder<StandardResponses>,
    coordinate: Option<SweepCoordinate>,
}

struct AttemptOutput {
    result: EvalResult,
    coordinate: Option<SweepCoordinate>,
}

#[derive(Clone)]
struct SweepCoordinate {
    agent: AgentId,
    trial: u16,
}

/// Immutable paths and task metadata available while configuring one attempt.
#[derive(Clone, Copy)]
pub struct EvalAttempt<'a> {
    task: &'a Task,
    directory: &'a Path,
    workspace: &'a Path,
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

    #[error("Nanocodex failed: {0}")]
    Nanocodex(#[from] NanocodexError),

    #[error("failed to configure attempt agent: {0}")]
    AttemptAgent(#[source] AttemptError),

    #[error("attempt verifier failed: {0}")]
    AttemptVerifier(#[source] AttemptError),

    #[error("agent exceeded its {0:?} timeout")]
    AgentTimeout(Duration),

    #[error("verifier exceeded its {0:?} timeout")]
    VerifierTimeout(Duration),

    #[error("agent event stream closed before a terminal event")]
    AgentEventsClosed,

    #[error("failed to encode or decode JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("evaluation job is already bound to a different run: {0}")]
    RunConflict(PathBuf),

    #[error("invalid verifier reward: {0}")]
    ParseReward(#[from] ParseFloatError),

    #[error("Nanoeval's concurrency semaphore was unexpectedly closed")]
    ConcurrencyClosed(#[from] AcquireError),

    #[error("sweep execution lost its task-agent-trial coordinates")]
    MissingSweepCoordinate,
}

impl Nanoeval {
    #[must_use]
    pub fn builder(nanocodex: NanocodexBuilder<StandardResponses>) -> NanoevalBuilder {
        NanoevalBuilder {
            nanocodex: nanocodex.shared_prompt_cache(),
            output_directory: PathBuf::from("nanoeval-runs"),
            max_concurrency: 1,
            attempt_agent: None,
        }
    }

    /// Runs one independent attempt.
    ///
    /// # Errors
    ///
    /// Returns an error when setup, the agent, or verification fails.
    pub async fn task(&self, task: Task) -> Result<EvalResult, EvalError> {
        let _permit = Arc::clone(&self.inner.concurrency).acquire_owned().await?;
        self.run_task(AttemptInput {
            task,
            nanocodex: self.inner.nanocodex.clone(),
            coordinate: None,
        })
        .await
        .map(|output| output.result)
    }

    /// Runs `count` fresh attempts of the same immutable task.
    ///
    /// Results preserve attempt order even when work completes out of order.
    ///
    /// # Errors
    ///
    /// Returns the first setup, agent, or verifier error.
    pub async fn task_n(&self, task: Task, count: usize) -> Result<Vec<EvalResult>, EvalError> {
        self.tasks(std::iter::repeat_n(task, count).collect()).await
    }

    /// Runs one independent attempt for every task in `tasks`.
    ///
    /// # Errors
    ///
    /// Returns the first setup, agent, or verifier error.
    pub async fn tasks(&self, tasks: Vec<Task>) -> Result<Vec<EvalResult>, EvalError> {
        let inputs = tasks
            .into_iter()
            .map(|task| AttemptInput {
                task,
                nanocodex: self.inner.nanocodex.clone(),
                coordinate: None,
            })
            .collect();
        Ok(self
            .run_tasks(inputs)
            .await?
            .into_iter()
            .map(|output| output.result)
            .collect())
    }

    /// Runs `count` fresh attempts for every task in `tasks`.
    ///
    /// Results are grouped in input task order and then trial order.
    ///
    /// # Errors
    ///
    /// Returns the first setup, agent, or verifier error.
    pub async fn tasks_n(
        &self,
        tasks: Vec<Task>,
        count: usize,
    ) -> Result<Vec<EvalResult>, EvalError> {
        self.tasks(
            tasks
                .into_iter()
                .flat_map(|task| std::iter::repeat_n(task, count))
                .collect(),
        )
        .await
    }

    /// Runs an advanced finite task-by-agent-by-trial sweep.
    ///
    /// # Errors
    ///
    /// Returns the first setup, agent, or verifier error.
    pub async fn sweep(&self, sweep: Sweep) -> Result<SweepResults, EvalError> {
        self.inner.job.bind_run(&sweep.manifest())?;
        let inputs = sweep
            .attempts()
            .map(|attempt| AttemptInput {
                task: attempt.task().clone(),
                nanocodex: attempt.nanocodex().clone(),
                coordinate: Some(SweepCoordinate {
                    agent: attempt.agent_id().clone(),
                    trial: attempt.trial(),
                }),
            })
            .collect();
        let attempts = self
            .run_tasks(inputs)
            .await?
            .into_iter()
            .map(|output| {
                let coordinate = output.coordinate.ok_or(EvalError::MissingSweepCoordinate)?;
                Ok(SweepAttemptResult::new(
                    coordinate.agent,
                    coordinate.trial,
                    output.result,
                ))
            })
            .collect::<Result<Vec<_>, EvalError>>()?;
        Ok(SweepResults::new(attempts))
    }

    async fn run_tasks(&self, tasks: Vec<AttemptInput>) -> Result<Vec<AttemptOutput>, EvalError> {
        let evaluator = self.clone();
        let mut completed = stream::iter(tasks.into_iter().enumerate())
            .map(move |(index, input)| {
                let evaluator = evaluator.clone();
                async move {
                    let result = async {
                        let _permit = Arc::clone(&evaluator.inner.concurrency)
                            .acquire_owned()
                            .await?;
                        evaluator.run_task(input).await
                    }
                    .await;
                    (index, result)
                }
            })
            .buffer_unordered(self.inner.max_concurrency);
        let mut results = Vec::new();
        let mut first_error = None;
        while let Some((index, result)) = completed.next().await {
            match result {
                Ok(result) => results.push((index, result)),
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        results.sort_unstable_by_key(|(index, _)| *index);
        Ok(results.into_iter().map(|(_, result)| result).collect())
    }

    /// Returns the stable identifier shared by this evaluator's attempts.
    #[must_use]
    pub fn id(&self) -> Uuid {
        self.inner.job.id()
    }

    /// Returns when this evaluator was built.
    #[must_use]
    pub fn started_at(&self) -> DateTime<Utc> {
        self.inner.job.started_at()
    }

    /// Returns the directory containing this evaluator's attempt artifacts.
    #[must_use]
    pub fn directory(&self) -> &std::path::Path {
        self.inner.job.directory()
    }

    /// Returns the parent directory containing evaluation jobs.
    #[must_use]
    pub fn parent_directory(&self) -> &std::path::Path {
        self.inner.job.parent_directory()
    }

    /// Returns the maximum number of concurrently executing attempts.
    #[must_use]
    pub fn max_concurrency(&self) -> usize {
        self.inner.max_concurrency
    }

    async fn run_task(&self, input: AttemptInput) -> Result<AttemptOutput, EvalError> {
        let AttemptInput {
            task,
            nanocodex,
            coordinate,
        } = input;
        let attempt_id = Uuid::new_v4();
        let prompt_cache_cohort = self
            .inner
            .next_prompt_cache_attempt
            .fetch_add(1, Ordering::Relaxed)
            / PROMPT_CACHE_COHORT_SIZE;
        let trial_name = trial_name(&task, attempt_id, coordinate.as_ref());
        let started_at = Utc::now();
        let mut emitter =
            AttemptEmitter::new(self, attempt_id, prompt_cache_cohort, &task, &trial_name);
        let span = attempt_span(
            self,
            &task,
            attempt_id,
            &trial_name,
            prompt_cache_cohort,
            coordinate.as_ref(),
        );
        record_content(&span, "task.prompt", task.prompt());
        let trace_started = Instant::now();
        let result = self
            .run_task_inner(
                task.clone(),
                nanocodex,
                attempt_id,
                trial_name.clone(),
                started_at,
                &mut emitter,
            )
            .instrument(span.clone())
            .await;
        record_attempt_result(&span, trace_started, &result);
        if let Err(error) = &result {
            emitter.emit(EvalEventKind::Failed(Box::new(attempt_failure(
                self, attempt_id, task, trial_name, started_at, error,
            ))));
        }
        result.map(|result| AttemptOutput { result, coordinate })
    }

    async fn run_task_inner(
        &self,
        task: Task,
        nanocodex: NanocodexBuilder<StandardResponses>,
        attempt_id: Uuid,
        trial_name: String,
        started_at: DateTime<Utc>,
        emitter: &mut AttemptEmitter<'_>,
    ) -> Result<EvalResult, EvalError> {
        let attempt = {
            let span = info_span!(
                target: "nanoeval",
                "eval.environment.setup",
                otel.kind = "internal",
                otel.status_code = tracing::field::Empty,
                eval.task.name = task.name(),
                eval.trial.name = trial_name.as_str(),
                output.directory = %self.inner.job.directory().display(),
                status = tracing::field::Empty,
                error.message = tracing::field::Empty,
                duration_ns = tracing::field::Empty,
            );
            let trace_started = Instant::now();
            let result = span.in_scope(|| {
                NativeAttempt::prepare(self.inner.job.directory(), &trial_name, &task)
            });
            record_span_result(&span, trace_started, &result);
            result?
        };
        emitter.emit(EvalEventKind::AttemptStarted {
            prompt: task.prompt().to_owned(),
            workspace: attempt.paths.workspace.clone(),
        });
        let mut agent = self
            .execute_agent(emitter, &task, &attempt, nanocodex)
            .await?;

        emitter.emit(EvalEventKind::VerifierStarted);
        let verifier = self
            .execute_verifier(&task, &attempt, agent.verifier.take())
            .await?;
        emitter.emit(EvalEventKind::VerifierOutput {
            stdout: verifier.stdout.clone(),
            stderr: verifier.stderr.clone(),
        });
        emitter.emit(EvalEventKind::VerifierCompleted(verifier.result.clone()));

        let result = EvalResult {
            attempt_id,
            task_name: task.name().to_owned(),
            trial_name,
            status: verifier_status(&verifier.result),
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
                verifier_output: attempt.paths.verifier_output.clone(),
            },
            task,
        };
        emitter.emit(EvalEventKind::Completed(Box::new(result.clone())));
        Ok(result)
    }

    async fn execute_verifier(
        &self,
        task: &Task,
        attempt: &NativeAttempt,
        verifier: Option<Box<dyn AttemptVerifier>>,
    ) -> Result<VerifierExecution, EvalError> {
        let span = info_span!(
            target: "nanoeval",
            "eval.verifier",
            otel.kind = "internal",
            otel.status_code = tracing::field::Empty,
            eval.task.name = task.name(),
            verifier.script = %task.verifier().script().display(),
            verifier.timeout_ms = duration_ms(task.verifier().timeout()),
            process.exit.code = tracing::field::Empty,
            verifier.reward.total = tracing::field::Empty,
            verifier.passed = tracing::field::Empty,
            verifier.stdout.bytes = tracing::field::Empty,
            verifier.stderr.bytes = tracing::field::Empty,
            status = tracing::field::Empty,
            error.message = tracing::field::Empty,
            duration_ns = tracing::field::Empty,
        );
        let trace_started = Instant::now();
        let result = async {
            if let Some(mut verifier) = verifier {
                let started_at = Utc::now();
                let execution = verifier
                    .verify(
                        task,
                        EvalAttempt {
                            task,
                            directory: &attempt.paths.root,
                            workspace: &attempt.paths.workspace,
                        },
                    )
                    .await
                    .map_err(EvalError::AttemptVerifier)?;
                Ok(VerifierExecution {
                    result: execution.result,
                    timing: PhaseTiming::finished(started_at),
                    stdout: execution.stdout,
                    stderr: execution.stderr,
                })
            } else {
                attempt.verify(task).await
            }
        }
        .instrument(span.clone())
        .await;
        if let Ok(verifier) = &result {
            let passed = verifier.result.rewards.values().all(|reward| *reward > 0.0);
            span.record("process.exit.code", verifier.result.exit_code);
            span.record(
                "verifier.reward.total",
                verifier.result.rewards.values().sum::<f64>(),
            );
            span.record("verifier.passed", passed);
            span.record("verifier.stdout.bytes", verifier.stdout.len());
            span.record("verifier.stderr.bytes", verifier.stderr.len());
            record_content(&span, "verifier.stdout", &verifier.stdout);
            record_content(&span, "verifier.stderr", &verifier.stderr);
        }
        record_span_result(&span, trace_started, &result);
        result
    }

    async fn execute_agent(
        &self,
        emitter: &mut AttemptEmitter<'_>,
        task: &Task,
        attempt: &NativeAttempt,
        nanocodex: NanocodexBuilder<StandardResponses>,
    ) -> Result<AgentExecution, EvalError> {
        let (agent, mut events, verifier, setup_timing) = {
            let setup_started = Utc::now();
            let span = info_span!(
                target: "nanoeval",
                "eval.agent.setup",
                otel.kind = "internal",
                otel.status_code = tracing::field::Empty,
                eval.task.name = task.name(),
                eval.attempt.id = %emitter.attempt_id,
                workspace = %attempt.paths.workspace.display(),
                status = tracing::field::Empty,
                error.message = tracing::field::Empty,
                duration_ns = tracing::field::Empty,
            );
            let trace_started = Instant::now();
            let result = span.in_scope(|| -> Result<_, EvalError> {
                let builder = nanocodex
                    .workspace(&attempt.paths.workspace)
                    .session_id(emitter.attempt_id.to_string())
                    .prompt_cache_key(format!(
                        "nanoeval:{}:{:x}",
                        self.id().simple(),
                        emitter.prompt_cache_cohort
                    ));
                let configured = if let Some(factory) = &self.inner.attempt_agent {
                    factory(
                        EvalAttempt {
                            task,
                            directory: &attempt.paths.root,
                            workspace: &attempt.paths.workspace,
                        },
                        builder,
                    )
                    .map_err(EvalError::AttemptAgent)?
                } else {
                    AttemptAgent::new(builder)
                };
                let (builder, verifier) = configured.into_parts();
                let (agent, events) = builder.build()?;
                Ok((agent, events, verifier))
            });
            record_span_result(&span, trace_started, &result);
            let (agent, events, verifier) = result?;
            (
                agent,
                events,
                verifier,
                PhaseTiming::finished(setup_started),
            )
        };
        let execution_started = Utc::now();
        let span = info_span!(
            target: "nanoeval",
            "eval.agent.execution",
            otel.kind = "internal",
            otel.status_code = tracing::field::Empty,
            eval.task.name = task.name(),
            eval.attempt.id = %emitter.attempt_id,
            agent.timeout_ms = duration_ms(task.agent_timeout()),
            status = tracing::field::Empty,
            error.message = tracing::field::Empty,
            duration_ns = tracing::field::Empty,
        );
        let trace_started = Instant::now();
        let result = async {
            let turn = agent.prompt(task.prompt()).await?;
            let control = turn.control();
            let event_result = timeout(task.agent_timeout(), async {
                loop {
                    let event = events.recv().await.ok_or(EvalError::AgentEventsClosed)?;
                    let terminal = event.kind.is_terminal();
                    emitter.emit(EvalEventKind::Agent(event.clone()));
                    if terminal {
                        let result = turn.result().await?;
                        return Ok::<_, EvalError>((result, event));
                    }
                }
            })
            .await;
            if let Ok(result) = event_result {
                result
            } else {
                let _ = control.cancel().await;
                Err(EvalError::AgentTimeout(task.agent_timeout()))
            }
        };
        let result = result.instrument(span.clone()).await;
        record_span_result(&span, trace_started, &result);
        let (turn_result, terminal_event) = result?;
        drop(agent);
        Ok(AgentExecution {
            result: AgentResult::from_terminal(turn_result.final_message, &terminal_event)?,
            verifier,
            setup_timing,
            execution_timing: PhaseTiming::finished(execution_started),
        })
    }
}

struct AgentExecution {
    result: AgentResult,
    verifier: Option<Box<dyn AttemptVerifier>>,
    setup_timing: PhaseTiming,
    execution_timing: PhaseTiming,
}

impl NanoevalBuilder {
    /// Sets the parent under which this evaluator creates one UUID-named
    /// artifact directory.
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

    /// Configures the fresh Nanocodex builder for each attempt.
    ///
    /// The factory runs after the disposable workspace is populated and before
    /// the agent is built. This is the boundary for attempt-owned resources
    /// such as a retained VM tool session and its guest-visible workspace.
    #[must_use]
    pub fn attempt_agent<F, E>(mut self, factory: F) -> Self
    where
        F: for<'a> Fn(
                EvalAttempt<'a>,
                NanocodexBuilder<StandardResponses>,
            ) -> Result<AttemptAgent, E>
            + Send
            + Sync
            + 'static,
        E: Error + Send + Sync + 'static,
    {
        self.attempt_agent = Some(Arc::new(move |attempt, builder| {
            factory(attempt, builder).map_err(|error| Box::new(error) as AttemptError)
        }));
        self
    }

    /// Builds a reusable evaluator and a source of independent event streams.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid concurrency or an unavailable output path.
    pub fn build(self) -> Result<(Nanoeval, NanoevalEvents), EvalError> {
        if self.max_concurrency == 0 {
            return Err(EvalError::InvalidConcurrency);
        }
        let job = EvalJob::create(&self.output_directory)?;
        let (event_sender, _) = broadcast::channel(EVENT_CAPACITY);
        Ok((
            Nanoeval {
                inner: Arc::new(NanoevalInner {
                    nanocodex: self.nanocodex,
                    job,
                    concurrency: Arc::new(Semaphore::new(self.max_concurrency)),
                    max_concurrency: self.max_concurrency,
                    next_prompt_cache_attempt: AtomicU64::new(0),
                    events: event_sender.clone(),
                    attempt_agent: self.attempt_agent,
                }),
            },
            NanoevalEvents::new(event_sender),
        ))
    }
}

impl AttemptAgent {
    #[must_use]
    pub fn new(nanocodex: NanocodexBuilder<StandardResponses>) -> Self {
        Self {
            nanocodex,
            verifier: None,
        }
    }

    #[must_use]
    pub fn verifier(mut self, verifier: impl AttemptVerifier + 'static) -> Self {
        self.verifier = Some(Box::new(verifier));
        self
    }

    fn into_parts(
        self,
    ) -> (
        NanocodexBuilder<StandardResponses>,
        Option<Box<dyn AttemptVerifier>>,
    ) {
        (self.nanocodex, self.verifier)
    }
}

impl EvalAttempt<'_> {
    #[must_use]
    pub const fn task(&self) -> &Task {
        self.task
    }

    #[must_use]
    pub const fn directory(&self) -> &Path {
        self.directory
    }

    #[must_use]
    pub const fn workspace(&self) -> &Path {
        self.workspace
    }
}

struct AttemptEmitter<'a> {
    eval: &'a Nanoeval,
    attempt_id: Uuid,
    prompt_cache_cohort: u64,
    task_name: String,
    trial_name: String,
    sequence: u64,
}

impl<'a> AttemptEmitter<'a> {
    fn new(
        eval: &'a Nanoeval,
        attempt_id: Uuid,
        prompt_cache_cohort: u64,
        task: &Task,
        trial_name: &str,
    ) -> Self {
        Self {
            eval,
            attempt_id,
            prompt_cache_cohort,
            task_name: task.name().to_owned(),
            trial_name: trial_name.to_owned(),
            sequence: 0,
        }
    }

    fn emit(&mut self, kind: EvalEventKind) {
        self.sequence += 1;
        let _ = self.eval.inner.events.send(Arc::new(EvalEvent {
            run_id: self.eval.inner.job.id(),
            attempt_id: self.attempt_id,
            task_name: self.task_name.clone(),
            trial_name: self.trial_name.clone(),
            sequence: self.sequence,
            kind,
        }));
    }
}

#[derive(Deserialize)]
struct ResponsesApiErrorEnvelope {
    error: ResponsesApiError,
}

#[derive(Deserialize)]
struct ResponsesApiError {
    code: Option<String>,
}

fn attempt_failure(
    eval: &Nanoeval,
    attempt_id: Uuid,
    task: Task,
    trial_name: String,
    started_at: DateTime<Utc>,
    error: &EvalError,
) -> EvalFailure {
    let root = eval.directory().join(&trial_name);
    EvalFailure {
        attempt_id,
        task_name: task.name().to_owned(),
        trial_name,
        kind: failure_kind(error),
        message: error.to_string(),
        traceback: error_traceback(error),
        model: MODEL.to_owned(),
        effort: "unknown".to_owned(),
        started_at,
        occurred_at: Utc::now(),
        artifacts: EvalArtifacts {
            workspace: root.join("workspace"),
            verifier_output: root.join("verifier/test-stdout.txt"),
            directory: root,
        },
        task,
    }
}

fn failure_kind(error: &EvalError) -> EvalFailureKind {
    match error {
        EvalError::Nanocodex(error) if is_safety_refusal(error) => {
            EvalFailureKind::AgentSafetyRefusal
        }
        EvalError::Nanocodex(error)
            if error
                .responses_error()
                .is_some_and(|error| error.class() == "authorization") =>
        {
            EvalFailureKind::AgentAuthentication
        }
        EvalError::AgentTimeout(_) => EvalFailureKind::AgentTimeout,
        EvalError::VerifierTimeout(_) => EvalFailureKind::VerifierTimeout,
        EvalError::Nanocodex(_) | EvalError::AgentEventsClosed => EvalFailureKind::Agent,
        EvalError::AttemptVerifier(_) | EvalError::ParseReward(_) => EvalFailureKind::Verifier,
        EvalError::UnsupportedNativeTask { .. }
        | EvalError::UnsupportedEnvironmentEntry(_)
        | EvalError::AttemptAgent(_) => EvalFailureKind::Environment,
        EvalError::InvalidConcurrency
        | EvalError::Io(_)
        | EvalError::Json(_)
        | EvalError::RunConflict(_)
        | EvalError::ConcurrencyClosed(_)
        | EvalError::MissingSweepCoordinate => EvalFailureKind::Internal,
    }
}

fn is_safety_refusal(error: &NanocodexError) -> bool {
    let Some(ResponsesError::Api { event }) = error.responses_error() else {
        return false;
    };
    serde_json::from_str::<ResponsesApiErrorEnvelope>(event)
        .ok()
        .and_then(|event| event.error.code)
        .is_some_and(|code| code == "cyber_policy")
}

fn error_traceback(error: &dyn Error) -> String {
    let mut traceback = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        traceback.push_str("\nCaused by: ");
        traceback.push_str(&error.to_string());
        source = error.source();
    }
    traceback
}

fn attempt_span(
    eval: &Nanoeval,
    task: &Task,
    attempt_id: Uuid,
    trial_name: &str,
    prompt_cache_cohort: u64,
    coordinate: Option<&SweepCoordinate>,
) -> Span {
    let span = info_span!(
        target: "nanoeval",
        parent: None,
        "eval.attempt",
        otel.kind = "internal",
        otel.status_code = tracing::field::Empty,
        eval.id = %eval.id(),
        eval.attempt.id = %attempt_id,
        eval.task.name = task.name(),
        eval.trial.name = trial_name,
        eval.agent.id = tracing::field::Empty,
        eval.trial.number = tracing::field::Empty,
        eval.task.image = task.image().reference(),
        eval.resource.cpus = task.resources().cpus,
        eval.resource.memory_mib = task.resources().memory_mb,
        eval.resource.storage_mib = task.resources().storage_mb,
        eval.resource.gpus = task.resources().gpus,
        eval.network = task.network().as_str(),
        eval.score.status = tracing::field::Empty,
        eval.reward.total = tracing::field::Empty,
        agent.model_calls = tracing::field::Empty,
        agent.tool_calls = tracing::field::Empty,
        agent.response_attempts = tracing::field::Empty,
        agent.response_retries = tracing::field::Empty,
        agent.prompt_cache.cohort = prompt_cache_cohort,
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.cached_input_tokens = tracing::field::Empty,
        gen_ai.usage.cache_write_input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        gen_ai.usage.total_tokens = tracing::field::Empty,
        agent.warmup.duration_ns = tracing::field::Empty,
        agent.warmup.input_tokens = tracing::field::Empty,
        agent.warmup.cached_input_tokens = tracing::field::Empty,
        agent.warmup.cache_write_input_tokens = tracing::field::Empty,
        agent.warmup.output_tokens = tracing::field::Empty,
        agent.warmup.total_tokens = tracing::field::Empty,
        cost.usd = tracing::field::Empty,
        status = tracing::field::Empty,
        error.message = tracing::field::Empty,
        duration_ns = tracing::field::Empty,
    );
    if let Some(coordinate) = coordinate {
        span.record("eval.agent.id", coordinate.agent.as_str());
        span.record("eval.trial.number", coordinate.trial);
    }
    span
}

fn record_attempt_result(span: &Span, started_at: Instant, result: &Result<EvalResult, EvalError>) {
    let duration_ns = elapsed_ns(started_at);
    span.record("duration_ns", duration_ns);
    match result {
        Ok(result) => {
            record_attempt_success(span, result);
            span.in_scope(|| {
                info!(
                    target: "nanoeval",
                    duration_ns,
                    score.status = eval_status(result.status),
                    "evaluation attempt completed"
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
                    error = %error,
                    "evaluation attempt failed"
                );
            });
        }
    }
}

fn record_attempt_success(span: &Span, result: &EvalResult) {
    let usage = &result.agent.usage;
    let warmup = &result.agent.metadata.warmup_usage;
    span.record("status", "completed");
    span.record("otel.status_code", "OK");
    span.record("eval.score.status", eval_status(result.status));
    span.record(
        "eval.reward.total",
        result.verifier.rewards.values().sum::<f64>(),
    );
    span.record("agent.model_calls", result.agent.model_calls);
    span.record("agent.tool_calls", result.agent.tool_calls);
    span.record(
        "agent.response_attempts",
        result.agent.metadata.response_attempts,
    );
    span.record(
        "agent.response_retries",
        result.agent.metadata.response_retries,
    );
    span.record("gen_ai.usage.input_tokens", usage.input_tokens);
    span.record(
        "gen_ai.usage.cached_input_tokens",
        usage.cached_input_tokens,
    );
    span.record(
        "gen_ai.usage.cache_write_input_tokens",
        usage.cache_write_input_tokens,
    );
    span.record("gen_ai.usage.output_tokens", usage.output_tokens);
    span.record("gen_ai.usage.total_tokens", usage.total_tokens);
    span.record(
        "agent.warmup.duration_ns",
        result.agent.metadata.warmup_duration_ns,
    );
    span.record("agent.warmup.input_tokens", warmup.input_tokens);
    span.record(
        "agent.warmup.cached_input_tokens",
        warmup.cached_input_tokens,
    );
    span.record(
        "agent.warmup.cache_write_input_tokens",
        warmup.cache_write_input_tokens,
    );
    span.record("agent.warmup.output_tokens", warmup.output_tokens);
    span.record("agent.warmup.total_tokens", warmup.total_tokens);
    if let Some(cost_usd) = result.agent.cost_usd {
        span.record("cost.usd", cost_usd);
    }
}

const fn eval_status(status: EvalStatus) -> &'static str {
    match status {
        EvalStatus::Passed => "passed",
        EvalStatus::Failed => "failed",
    }
}

fn verifier_status(verifier: &crate::VerifierResult) -> EvalStatus {
    if verifier.rewards.values().all(|reward| *reward > 0.0) {
        EvalStatus::Passed
    } else {
        EvalStatus::Failed
    }
}

fn record_span_result<T, E>(span: &tracing::Span, started_at: Instant, result: &Result<T, E>)
where
    E: std::fmt::Display,
{
    let duration_ns = elapsed_ns(started_at);
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
                    "evaluation phase completed"
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
                    "evaluation phase failed"
                );
            });
        }
    }
}

fn record_content(span: &tracing::Span, kind: &'static str, content: &str) {
    span.in_scope(|| {
        info!(
            target: "nanoeval",
            content_kind = kind,
            content,
            "evaluation content"
        );
    });
}

fn elapsed_ns(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn trial_name(task: &Task, attempt_id: Uuid, coordinate: Option<&SweepCoordinate>) -> String {
    let short_name = task.name().rsplit('/').next().unwrap_or(task.name());
    let compact_id = attempt_id.simple().to_string();
    match coordinate {
        Some(coordinate) => format!(
            "{short_name}__{}__{:03}__{}",
            coordinate.agent,
            coordinate.trial,
            &compact_id[..8]
        ),
        None => format!("{short_name}__{}", &compact_id[..8]),
    }
}

impl AgentResult {
    fn from_terminal(final_message: String, event: &AgentEvent) -> Result<Self, EvalError> {
        if event.kind != AgentEventKind::RunCompleted {
            return Err(EvalError::AgentEventsClosed);
        }
        let metadata: AgentMetadata = serde_json::from_str(event.payload.get())?;
        Ok(Self {
            final_message,
            model: metadata.model.clone(),
            effort: metadata.effort.clone(),
            model_calls: metadata.model_calls,
            tool_calls: metadata.tool_calls,
            usage: metadata.usage.clone(),
            cost_usd: metadata.cost_usd,
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

#[cfg(test)]
mod tracing_tests {
    use std::{
        collections::HashMap,
        fs,
        sync::{Arc, Mutex},
    };

    use nanocodex::{Nanocodex, NanocodexError, ResponsesError};
    use tempfile::tempdir;
    use tracing::{Id, Instrument, Subscriber, field::Visit, span::Attributes};
    use tracing_subscriber::{
        Layer, layer::Context as LayerContext, prelude::*, registry::LookupSpan,
    };

    use super::{EvalError, Nanoeval, failure_kind};
    use crate::{EvalFailureKind, Task};

    #[derive(Clone, Default)]
    struct TraceCapture(Arc<Mutex<HashMap<u64, CapturedSpan>>>);

    struct CapturedSpan {
        name: &'static str,
        parent: Option<u64>,
        fields: HashMap<String, String>,
    }

    struct FieldCapture<'a>(&'a mut HashMap<String, String>);

    impl Visit for FieldCapture<'_> {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.insert(field.name().to_owned(), value.to_owned());
        }

        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().to_owned(), format!("{value:?}"));
        }
    }

    impl<S> Layer<S> for TraceCapture
    where
        S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    {
        fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, context: LayerContext<'_, S>) {
            let parent = attributes
                .parent()
                .map(|parent| parent.clone().into_u64())
                .or_else(|| {
                    attributes
                        .is_contextual()
                        .then(|| context.current_span().id().map(Id::into_u64))
                        .flatten()
                });
            let mut fields = HashMap::new();
            attributes.record(&mut FieldCapture(&mut fields));
            self.0.lock().unwrap().insert(
                id.clone().into_u64(),
                CapturedSpan {
                    name: attributes.metadata().name(),
                    parent,
                    fields,
                },
            );
        }

        fn on_record(
            &self,
            id: &Id,
            values: &tracing::span::Record<'_>,
            _context: LayerContext<'_, S>,
        ) {
            if let Some(span) = self.0.lock().unwrap().get_mut(&id.clone().into_u64()) {
                values.record(&mut FieldCapture(&mut span.fields));
            }
        }
    }

    #[test]
    fn failed_attempt_does_not_cancel_pending_batch_work() {
        let task_root = tempdir().unwrap();
        fs::create_dir(task_root.path().join("tests")).unwrap();
        fs::create_dir(task_root.path().join("environment")).unwrap();
        fs::write(
            task_root.path().join("task.toml"),
            r#"
schema_version = "1.1"
[task]
name = "terminal-bench/traced"
description = "Tracing fixture"
[metadata]
custom_docker_compose = true
[agent]
timeout_sec = 1.0
[verifier]
timeout_sec = 1.0
[environment]
docker_image = "example/traced:latest"
cpus = 1
memory_mb = 128
storage_mb = 128
gpus = 0
allow_internet = false
"#,
        )
        .unwrap();
        fs::write(
            task_root.path().join("instruction.md"),
            "do the traced work\n",
        )
        .unwrap();
        fs::write(task_root.path().join("tests/test.sh"), "exit 0\n").unwrap();
        let task = Task::load(task_root.path()).unwrap();
        let output = tempdir().unwrap();
        let capture = TraceCapture::default();
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(capture.clone()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        tracing::dispatcher::with_default(&dispatch, || {
            runtime.block_on(async {
                let (eval, _events) = Nanoeval::builder(Nanocodex::builder("test"))
                    .output_directory(output.path())
                    .build()
                    .unwrap();
                let result = eval
                    .tasks(vec![task.clone(), task])
                    .instrument(tracing::info_span!("test.parent"))
                    .await;
                assert!(matches!(
                    result,
                    Err(EvalError::UnsupportedNativeTask { .. })
                ));
            });
        });

        let spans = capture.0.lock().unwrap();
        let attempts = spans
            .iter()
            .filter(|(_, span)| span.name == "eval.attempt")
            .collect::<Vec<_>>();
        assert_eq!(attempts.len(), 2);
        for (_, attempt) in &attempts {
            assert!(attempt.parent.is_none());
            assert_eq!(
                attempt.fields.get("status").map(String::as_str),
                Some("failed")
            );
            assert!(attempt.fields.contains_key("duration_ns"));
        }
        let setups = spans
            .values()
            .filter(|span| span.name == "eval.environment.setup")
            .collect::<Vec<_>>();
        assert_eq!(setups.len(), 2);
        for setup in setups {
            assert!(
                attempts
                    .iter()
                    .any(|(attempt_id, _)| setup.parent == Some(**attempt_id))
            );
            assert_eq!(
                setup.fields.get("status").map(String::as_str),
                Some("failed")
            );
            assert!(setup.fields.contains_key("duration_ns"));
        }
    }

    #[test]
    fn classifies_cyber_policy_as_an_agent_safety_refusal() {
        let error = EvalError::Nanocodex(NanocodexError::Responses(ResponsesError::Api {
            event: r#"{"type":"error","error":{"code":"cyber_policy"}}"#.to_owned(),
        }));

        assert_eq!(failure_kind(&error), EvalFailureKind::AgentSafetyRefusal);
    }
}

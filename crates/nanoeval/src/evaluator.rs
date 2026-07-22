use std::{num::ParseFloatError, path::PathBuf, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use futures_util::{StreamExt, stream};
use nanocodex::{AgentEvent, AgentEventKind, NanocodexBuilder, NanocodexError, StandardResponses};
use tokio::{
    sync::{AcquireError, Semaphore, broadcast},
    time::timeout,
};
use uuid::Uuid;

use crate::{
    AgentMetadata, AgentResult, EvalArtifacts, EvalEvent, EvalEventKind, EvalResult, EvalStatus,
    EvalTiming, NanoevalEvents, PhaseTiming, Task, job::EvalJob, native::NativeAttempt,
};

const EVENT_CAPACITY: usize = 16_384;

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
    job: EvalJob,
    concurrency: Arc<Semaphore>,
    max_concurrency: usize,
    events: broadcast::Sender<Arc<EvalEvent>>,
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
    /// Returns an error when setup, the agent, or verification fails.
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
    /// Returns the first setup, agent, or verifier error.
    pub async fn task_n(&self, task: Task, count: usize) -> Result<Vec<EvalResult>, EvalError> {
        self.tasks(std::iter::repeat_n(task, count)).await
    }

    /// Runs independent tasks with the configured concurrency bound.
    ///
    /// # Errors
    ///
    /// Returns the first setup, agent, or verifier error.
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

    async fn run_task(&self, task: Task) -> Result<EvalResult, EvalError> {
        let started_at = Utc::now();
        let attempt_id = Uuid::new_v4();
        let trial_name = trial_name(&task, attempt_id);
        let attempt = NativeAttempt::prepare(self.inner.job.directory(), &trial_name, &task)?;
        let mut emitter = AttemptEmitter::new(self, attempt_id, &task, &trial_name);
        emitter.emit(EvalEventKind::AttemptStarted {
            prompt: task.prompt().to_owned(),
            workspace: attempt.paths.workspace.clone(),
        });
        let agent = self.execute_agent(&mut emitter, &task, &attempt).await?;

        emitter.emit(EvalEventKind::VerifierStarted);
        let verifier = attempt.verify(&task).await?;
        emitter.emit(EvalEventKind::VerifierOutput {
            stdout: verifier.stdout.clone(),
            stderr: verifier.stderr.clone(),
        });
        emitter.emit(EvalEventKind::VerifierCompleted(verifier.result.clone()));

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
                verifier_output: attempt.paths.verifier_output.clone(),
            },
            task,
        };
        emitter.emit(EvalEventKind::Completed(Box::new(result.clone())));
        Ok(result)
    }

    async fn execute_agent(
        &self,
        emitter: &mut AttemptEmitter<'_>,
        task: &Task,
        attempt: &NativeAttempt,
    ) -> Result<AgentExecution, EvalError> {
        let setup_started = Utc::now();
        let (agent, mut events) = self
            .inner
            .nanocodex
            .clone()
            .workspace(&attempt.paths.workspace)
            .session_id(emitter.attempt_id.to_string())
            .build()?;
        let setup_timing = PhaseTiming::finished(setup_started);
        let execution_started = Utc::now();
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
        let (turn_result, terminal_event) = if let Ok(result) = event_result {
            result?
        } else {
            let _ = control.cancel().await;
            return Err(EvalError::AgentTimeout(task.agent_timeout()));
        };
        drop(agent);
        Ok(AgentExecution {
            result: AgentResult::from_terminal(turn_result.final_message, &terminal_event)?,
            setup_timing,
            execution_timing: PhaseTiming::finished(execution_started),
        })
    }
}

struct AgentExecution {
    result: AgentResult,
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
                    events: event_sender.clone(),
                }),
            },
            NanoevalEvents::new(event_sender),
        ))
    }
}

struct AttemptEmitter<'a> {
    eval: &'a Nanoeval,
    attempt_id: Uuid,
    task_name: String,
    trial_name: String,
    sequence: u64,
}

impl<'a> AttemptEmitter<'a> {
    fn new(eval: &'a Nanoeval, attempt_id: Uuid, task: &Task, trial_name: &str) -> Self {
        Self {
            eval,
            attempt_id,
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

fn trial_name(task: &Task, attempt_id: Uuid) -> String {
    let short_name = task.name().rsplit('/').next().unwrap_or(task.name());
    let compact_id = attempt_id.simple().to_string();
    format!("{short_name}__{}", &compact_id[..8])
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

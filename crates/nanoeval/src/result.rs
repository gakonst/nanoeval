use std::{collections::BTreeMap, path::PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{AgentId, Task};

/// Terminal score classification for one attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalStatus {
    Passed,
    Failed,
}

/// Stable classification for an attempt that could not produce a score.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalFailureKind {
    AgentSafetyRefusal,
    AgentAuthentication,
    AgentTimeout,
    VerifierTimeout,
    Agent,
    Verifier,
    Environment,
    Internal,
}

/// Typed terminal output for an errored or refused attempt.
#[derive(Clone, Debug, Serialize)]
pub struct EvalFailure {
    pub attempt_id: Uuid,
    pub task_name: String,
    pub trial_name: String,
    pub kind: EvalFailureKind,
    pub message: String,
    pub traceback: String,
    pub model: String,
    pub effort: String,
    pub started_at: DateTime<Utc>,
    pub occurred_at: DateTime<Utc>,
    pub artifacts: EvalArtifacts,
    #[serde(skip)]
    pub(crate) task: Task,
}

/// Typed result returned by [`crate::Nanoeval::task`].
#[derive(Clone, Debug, Serialize)]
pub struct EvalResult {
    pub attempt_id: Uuid,
    pub task_name: String,
    pub trial_name: String,
    pub status: EvalStatus,
    pub agent: AgentResult,
    pub verifier: VerifierResult,
    pub timing: EvalTiming,
    pub artifacts: EvalArtifacts,
    #[serde(skip)]
    pub(crate) task: Task,
}

/// Results from an advanced task-by-agent-by-trial sweep.
#[derive(Clone, Debug, Serialize)]
pub struct SweepResults {
    attempts: Vec<SweepAttemptResult>,
}

/// One self-identifying result in a [`SweepResults`] collection.
#[derive(Clone, Debug, Serialize)]
pub struct SweepAttemptResult {
    agent: AgentId,
    trial: u16,
    result: EvalResult,
}

impl SweepResults {
    pub(crate) const fn new(attempts: Vec<SweepAttemptResult>) -> Self {
        Self { attempts }
    }

    #[must_use]
    pub fn attempts(&self) -> &[SweepAttemptResult] {
        &self.attempts
    }

    #[must_use]
    pub fn into_results(self) -> Vec<EvalResult> {
        self.attempts
            .into_iter()
            .map(|attempt| attempt.result)
            .collect()
    }
}

impl SweepAttemptResult {
    pub(crate) const fn new(agent: AgentId, trial: u16, result: EvalResult) -> Self {
        Self {
            agent,
            trial,
            result,
        }
    }

    #[must_use]
    pub fn task_name(&self) -> &str {
        &self.result.task_name
    }

    #[must_use]
    pub const fn agent(&self) -> &AgentId {
        &self.agent
    }

    #[must_use]
    pub const fn trial(&self) -> u16 {
        self.trial
    }

    #[must_use]
    pub const fn result(&self) -> &EvalResult {
        &self.result
    }

    #[must_use]
    pub fn into_result(self) -> EvalResult {
        self.result
    }
}

impl EvalResult {
    /// The immutable task definition used by this attempt.
    #[must_use]
    pub const fn task(&self) -> &Task {
        &self.task
    }
}

impl EvalFailure {
    /// The immutable task definition used by this attempt.
    #[must_use]
    pub const fn task(&self) -> &Task {
        &self.task
    }
}

impl EvalFailureKind {
    /// Harbor's exception class for this terminal failure.
    #[must_use]
    pub const fn harbor_exception_type(self) -> &'static str {
        match self {
            Self::AgentSafetyRefusal => "AgentSafetyRefusalError",
            Self::AgentAuthentication => "AgentAuthenticationError",
            Self::AgentTimeout => "AgentTimeoutError",
            Self::VerifierTimeout => "VerifierTimeoutError",
            Self::Agent => "AgentError",
            Self::Verifier => "VerifierError",
            Self::Environment => "EnvironmentError",
            Self::Internal => "NanoevalError",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct AgentResult {
    pub final_message: String,
    pub model: String,
    pub effort: String,
    pub model_calls: u32,
    pub tool_calls: u32,
    pub usage: UsageTotals,
    pub cost_usd: Option<f64>,
    pub metadata: AgentMetadata,
}

/// Typed metadata emitted by Nanocodex's terminal event.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentMetadata {
    pub status: AgentStatus,
    pub model: String,
    pub effort: String,
    pub transport: String,
    pub orchestration: String,
    pub duration_ms: u64,
    pub duration_ns: u64,
    pub model_calls: u32,
    pub steers: u32,
    pub compactions: u32,
    pub tool_calls: u32,
    pub connection_attempts: u32,
    pub websocket_reconnects: u32,
    pub response_attempts: u32,
    pub response_retries: u32,
    pub connection_duration_ns: u64,
    pub retry_backoff_duration_ns: u64,
    pub model_duration_ns: u64,
    pub warmup_duration_ns: u64,
    pub tool_work_duration_ns: u64,
    pub tool_wall_duration_ns: u64,
    pub usage: UsageTotals,
    pub warmup_usage: UsageTotals,
    #[serde(default, rename = "last_response_id", skip_serializing)]
    _last_response_id: Option<String>,
    pub cost_usd: Option<f64>,
    pub cost_status: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct VerifierResult {
    pub exit_code: i32,
    pub rewards: BTreeMap<String, f64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct EvalTiming {
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub environment_setup: PhaseTiming,
    pub agent_setup: PhaseTiming,
    pub agent_execution: PhaseTiming,
    pub verifier: PhaseTiming,
}

#[derive(Clone, Debug, Serialize)]
pub struct PhaseTiming {
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize)]
pub struct EvalArtifacts {
    pub directory: PathBuf,
    pub workspace: PathBuf,
    pub verifier_output: PathBuf,
}

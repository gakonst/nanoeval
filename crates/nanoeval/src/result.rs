use std::{collections::BTreeMap, path::PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::AtifTrajectory;

/// Terminal score classification for one attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalStatus {
    Passed,
    Failed,
}

/// Typed result returned by [`crate::Nanoeval::task`].
#[derive(Clone, Debug, Serialize)]
pub struct EvalResult {
    pub attempt_id: Uuid,
    pub task_name: String,
    pub trial_name: String,
    pub status: EvalStatus,
    pub agent: AgentResult,
    pub trajectory: AtifTrajectory,
    pub verifier: VerifierResult,
    pub timing: EvalTiming,
    pub artifacts: EvalArtifacts,
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
    pub events_jsonl: PathBuf,
    pub trajectory_json: PathBuf,
    pub verifier_output: PathBuf,
    pub result_json: PathBuf,
}

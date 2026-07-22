use std::{collections::BTreeMap, path::PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

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
    pub metadata: Value,
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

use std::collections::BTreeMap;

use nanocodex::{AgentEvent, AgentEventKind, Usage};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::{AgentMetadata, AgentResult, Task, UsageTotals};

/// A complete ATIF-v1.7 projection of one agent attempt.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AtifTrajectory {
    pub schema_version: AtifSchemaVersion,
    pub session_id: String,
    pub agent: AtifAgent,
    pub steps: Vec<AtifStep>,
    pub final_metrics: AtifFinalMetrics,
}

impl AtifTrajectory {
    #[must_use]
    pub fn tool_call_count(&self) -> usize {
        self.steps
            .iter()
            .filter_map(|step| step.tool_calls.as_ref())
            .map(Vec::len)
            .sum()
    }

    #[must_use]
    pub fn observation_count(&self) -> usize {
        self.steps
            .iter()
            .filter_map(|step| step.observation.as_ref())
            .map(|observation| observation.results.len())
            .sum()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub enum AtifSchemaVersion {
    #[serde(rename = "ATIF-v1.7")]
    V1_7,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AtifAgent {
    pub name: String,
    pub version: String,
    pub model_name: String,
    pub extra: AtifAgentExtra,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AtifAgentExtra {
    pub transport: String,
    pub orchestration: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AtifStep {
    pub step_id: u32,
    pub source: AtifSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<AtifToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation: Option<AtifObservation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<AtifMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_call_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<AtifStepExtra>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AtifSource {
    User,
    Agent,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AtifToolCall {
    pub tool_call_id: String,
    pub function_name: String,
    pub arguments: Box<RawValue>,
    pub extra: AtifToolCallExtra,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AtifToolCallExtra {
    pub model_call_index: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AtifObservation {
    pub results: Vec<AtifObservationResult>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AtifObservationResult {
    pub source_call_id: String,
    pub content: String,
    pub extra: AtifObservationExtra,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AtifObservationExtra {
    pub status: String,
    pub duration_ns: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AtifMetrics {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    pub extra: AtifModelCallMetrics,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AtifModelCallMetrics {
    pub model_call_index: u32,
    pub attempt: u32,
    pub connection_generation: u32,
    pub duration_ns: u64,
    pub time_to_first_event_ns: u64,
    pub time_to_first_output_ns: Option<u64>,
    pub tool_calls: usize,
    pub cache_write_input_tokens: u64,
    pub reasoning_output_tokens: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AtifStepExtra {
    pub terminal_event_type: String,
    pub terminal_payload: AgentMetadata,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AtifFinalMetrics {
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_cached_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    pub total_steps: u32,
    pub extra: AtifFinalMetricsExtra,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AtifFinalMetricsExtra {
    pub model_calls: u32,
    pub tool_calls: u32,
    pub duration_ns: u64,
    #[serde(flatten)]
    pub runtime: AtifRuntimeMetrics,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AtifRuntimeMetrics {
    pub connection_attempts: u32,
    pub websocket_reconnects: u32,
    pub connection_duration_ns: u64,
    pub model_duration_ns: u64,
    pub warmup_duration_ns: u64,
    pub tool_work_duration_ns: u64,
    pub tool_wall_duration_ns: u64,
    pub warmup_usage: UsageTotals,
    pub cache_write_input_tokens: u64,
    pub reasoning_output_tokens: u64,
}

#[derive(Default)]
pub(crate) struct AtifBuilder {
    session_id: Option<String>,
    turns: BTreeMap<u32, AtifTurn>,
    tool_turns: BTreeMap<String, u32>,
}

#[derive(Default)]
struct AtifTurn {
    model_name: Option<String>,
    reasoning_effort: Option<String>,
    message: String,
    reasoning: String,
    tool_calls: Vec<AtifToolCall>,
    observations: Vec<AtifObservationResult>,
    metrics: Option<AtifMetrics>,
}

impl AtifBuilder {
    pub(crate) fn apply(&mut self, event: &AgentEvent) -> Result<(), serde_json::Error> {
        if self.session_id.is_none() {
            self.session_id = Some(event.request_id.to_string());
        }
        match event.kind {
            AgentEventKind::ModelCallStarted => {
                let payload = serde_json::from_str::<ModelCallStartedPayload>(event.payload.get())?;
                let turn = self.turns.entry(payload.call_index).or_default();
                turn.model_name = Some(payload.model);
                turn.reasoning_effort = Some(payload.effort);
            }
            AgentEventKind::AssistantMessage => {
                let payload = serde_json::from_str::<AssistantMessagePayload>(event.payload.get())?;
                append_message(
                    &mut self
                        .turns
                        .entry(payload.model_call_index)
                        .or_default()
                        .message,
                    &payload.text,
                );
            }
            AgentEventKind::ReasoningSummaryDelta => {
                let payload = serde_json::from_str::<ReasoningSummaryPayload>(event.payload.get())?;
                self.turns
                    .entry(payload.model_call_index)
                    .or_default()
                    .reasoning
                    .push_str(&payload.text);
            }
            AgentEventKind::ModelCallCompleted => {
                let payload =
                    serde_json::from_str::<ModelCallCompletedPayload>(event.payload.get())?;
                let metrics = payload
                    .usage
                    .as_ref()
                    .map(|usage| AtifMetrics::from_call(&payload, usage));
                let turn = self.turns.entry(payload.call_index).or_default();
                turn.model_name = Some(payload.model);
                turn.metrics = metrics;
            }
            AgentEventKind::ToolCall => {
                let payload = serde_json::from_str::<ToolCallPayload>(event.payload.get())?;
                let arguments = object_arguments(payload.arguments)?;
                self.tool_turns
                    .insert(payload.call_id.clone(), payload.model_call_index);
                self.turns
                    .entry(payload.model_call_index)
                    .or_default()
                    .tool_calls
                    .push(AtifToolCall {
                        tool_call_id: payload.call_id,
                        function_name: payload.tool,
                        arguments,
                        extra: AtifToolCallExtra {
                            model_call_index: payload.model_call_index,
                        },
                    });
            }
            AgentEventKind::ToolResult => {
                let payload = serde_json::from_str::<ToolResultPayload>(event.payload.get())?;
                if let Some(model_call_index) = self.tool_turns.get(&payload.call_id).copied() {
                    self.turns
                        .entry(model_call_index)
                        .or_default()
                        .observations
                        .push(AtifObservationResult {
                            source_call_id: payload.call_id,
                            content: payload.result.get().to_owned(),
                            extra: AtifObservationExtra {
                                status: payload.status,
                                duration_ns: payload.duration_ns,
                            },
                        });
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn finish(self, task: &Task, result: &AgentResult) -> AtifTrajectory {
        let runtime = AtifRuntimeMetrics::from(&result.metadata);
        let mut steps = Vec::with_capacity(self.turns.len() + 1);
        steps.push(AtifStep::user(1, task.prompt()));
        for (offset, turn) in self.turns.into_values().enumerate() {
            let step_id = u32::try_from(offset + 2).unwrap_or(u32::MAX);
            steps.push(turn.into_step(step_id, result));
        }
        if let Some(last) = steps
            .iter_mut()
            .rev()
            .find(|step| matches!(step.source, AtifSource::Agent))
        {
            if last.message.is_empty() {
                last.message.clone_from(&result.final_message);
            }
            last.extra = Some(AtifStepExtra {
                terminal_event_type: "run.completed".to_owned(),
                terminal_payload: result.metadata.clone(),
            });
        }
        let total_steps = u32::try_from(steps.len()).unwrap_or(u32::MAX);
        AtifTrajectory {
            schema_version: AtifSchemaVersion::V1_7,
            session_id: self.session_id.unwrap_or_default(),
            agent: AtifAgent {
                name: "nanocodex".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                model_name: result.model.clone(),
                extra: AtifAgentExtra {
                    transport: result.metadata.transport.clone(),
                    orchestration: result.metadata.orchestration.clone(),
                },
            },
            steps,
            final_metrics: AtifFinalMetrics {
                total_prompt_tokens: result.usage.input_tokens,
                total_completion_tokens: result.usage.output_tokens,
                total_cached_tokens: result.usage.cached_input_tokens,
                total_cost_usd: result.cost_usd,
                total_steps,
                extra: AtifFinalMetricsExtra {
                    model_calls: result.model_calls,
                    tool_calls: result.tool_calls,
                    duration_ns: result.metadata.duration_ns,
                    runtime,
                },
            },
        }
    }
}

impl AtifTurn {
    fn into_step(self, step_id: u32, result: &AgentResult) -> AtifStep {
        AtifStep {
            step_id,
            source: AtifSource::Agent,
            model_name: self.model_name.or_else(|| Some(result.model.clone())),
            reasoning_effort: self
                .reasoning_effort
                .or_else(|| Some(result.effort.clone())),
            message: self.message,
            reasoning_content: (!self.reasoning.is_empty()).then_some(self.reasoning),
            tool_calls: (!self.tool_calls.is_empty()).then_some(self.tool_calls),
            observation: (!self.observations.is_empty()).then_some(AtifObservation {
                results: self.observations,
            }),
            metrics: self.metrics,
            llm_call_count: Some(1),
            extra: None,
        }
    }
}

impl AtifStep {
    fn user(step_id: u32, message: &str) -> Self {
        Self {
            step_id,
            source: AtifSource::User,
            model_name: None,
            reasoning_effort: None,
            message: message.to_owned(),
            reasoning_content: None,
            tool_calls: None,
            observation: None,
            metrics: None,
            llm_call_count: None,
            extra: None,
        }
    }
}

impl AtifMetrics {
    fn from_call(call: &ModelCallCompletedPayload, usage: &Usage) -> Self {
        let cached_tokens = usage
            .input_tokens_details
            .as_ref()
            .map_or(0, |details| details.cached_tokens);
        let cache_write_input_tokens = usage
            .input_tokens_details
            .as_ref()
            .map_or(0, |details| details.cache_write_tokens);
        let reasoning_output_tokens = usage
            .output_tokens_details
            .as_ref()
            .map_or(0, |details| details.reasoning_tokens);
        Self {
            prompt_tokens: usage.input_tokens,
            completion_tokens: usage.output_tokens,
            cached_tokens,
            cost_usd: None,
            extra: AtifModelCallMetrics {
                model_call_index: call.call_index,
                attempt: call.attempt,
                connection_generation: call.connection_generation,
                duration_ns: call.duration_ns,
                time_to_first_event_ns: call.time_to_first_event_ns,
                time_to_first_output_ns: call.time_to_first_output_ns,
                tool_calls: call.tool_calls,
                cache_write_input_tokens,
                reasoning_output_tokens,
            },
        }
    }
}

impl From<&AgentMetadata> for AtifRuntimeMetrics {
    fn from(metadata: &AgentMetadata) -> Self {
        Self {
            connection_attempts: metadata.connection_attempts,
            websocket_reconnects: metadata.websocket_reconnects,
            connection_duration_ns: metadata.connection_duration_ns,
            model_duration_ns: metadata.model_duration_ns,
            warmup_duration_ns: metadata.warmup_duration_ns,
            tool_work_duration_ns: metadata.tool_work_duration_ns,
            tool_wall_duration_ns: metadata.tool_wall_duration_ns,
            warmup_usage: metadata.warmup_usage.clone(),
            cache_write_input_tokens: metadata.usage.cache_write_input_tokens,
            reasoning_output_tokens: metadata.usage.reasoning_output_tokens,
        }
    }
}

fn append_message(message: &mut String, next: &str) {
    if !message.is_empty() && !next.is_empty() {
        message.push_str("\n\n");
    }
    message.push_str(next);
}

fn object_arguments(arguments: Box<RawValue>) -> Result<Box<RawValue>, serde_json::Error> {
    if arguments.get().trim_start().starts_with('{') {
        return Ok(arguments);
    }
    RawValue::from_string(format!(r#"{{"raw":{}}}"#, arguments.get()))
}

#[derive(Deserialize)]
struct ModelCallStartedPayload {
    call_index: u32,
    model: String,
    effort: String,
}

#[derive(Deserialize)]
struct AssistantMessagePayload {
    model_call_index: u32,
    text: String,
}

#[derive(Deserialize)]
struct ReasoningSummaryPayload {
    model_call_index: u32,
    text: String,
}

#[derive(Deserialize)]
struct ModelCallCompletedPayload {
    call_index: u32,
    model: String,
    attempt: u32,
    connection_generation: u32,
    duration_ns: u64,
    time_to_first_event_ns: u64,
    time_to_first_output_ns: Option<u64>,
    tool_calls: usize,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct ToolCallPayload {
    call_id: String,
    tool: String,
    arguments: Box<RawValue>,
    model_call_index: u32,
}

#[derive(Deserialize)]
struct ToolResultPayload {
    call_id: String,
    status: String,
    duration_ns: u64,
    result: Box<RawValue>,
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use nanocodex::AgentEvent;

    use crate::{AgentMetadata, AgentResult, AtifSource, Task};

    use super::AtifBuilder;

    #[test]
    fn preserves_model_turns_and_their_tool_observations() {
        let events = [
            event(
                1,
                "model.call.started",
                r#"{"call_index":1,"model":"gpt-test","effort":"low"}"#,
            ),
            event(
                2,
                "assistant.message",
                r#"{"model_call_index":1,"text":"I will inspect."}"#,
            ),
            event(
                3,
                "model.call.completed",
                r#"{"call_index":1,"model":"gpt-test","attempt":1,"connection_generation":1,"duration_ns":10,"time_to_first_event_ns":2,"time_to_first_output_ns":3,"tool_calls":1,"usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":4,"cache_write_tokens":0},"output_tokens":2,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":12}}"#,
            ),
            event(
                4,
                "tool.call",
                r#"{"call_id":"call-1","tool":"exec","arguments":"pwd","model_call_index":1}"#,
            ),
            event(
                5,
                "tool.result",
                r#"{"call_id":"call-1","status":"completed","duration_ns":42,"result":"/workspace"}"#,
            ),
            event(
                6,
                "model.call.started",
                r#"{"call_index":2,"model":"gpt-test","effort":"low"}"#,
            ),
            event(
                7,
                "reasoning.summary.delta",
                r#"{"model_call_index":2,"text":"Done"}"#,
            ),
            event(
                8,
                "assistant.message",
                r#"{"model_call_index":2,"text":"Finished."}"#,
            ),
            event(
                9,
                "model.call.completed",
                r#"{"call_index":2,"model":"gpt-test","attempt":1,"connection_generation":1,"duration_ns":20,"time_to_first_event_ns":2,"time_to_first_output_ns":3,"tool_calls":0,"usage":{"input_tokens":12,"output_tokens":3,"total_tokens":15}}"#,
            ),
        ];
        let mut builder = AtifBuilder::default();
        for event in &events {
            builder.apply(event).unwrap();
        }
        let task =
            Task::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tasks/write-greeting"))
                .unwrap();
        let metadata: AgentMetadata = serde_json::from_str(TERMINAL_METADATA).unwrap();
        let result = AgentResult {
            final_message: "Finished.".to_owned(),
            model: metadata.model.clone(),
            effort: metadata.effort.clone(),
            model_calls: metadata.model_calls,
            tool_calls: metadata.tool_calls,
            usage: metadata.usage.clone(),
            cost_usd: metadata.cost_usd,
            metadata,
        };

        let trajectory = builder.finish(&task, &result);

        assert_eq!(trajectory.steps.len(), 3);
        assert!(matches!(trajectory.steps[0].source, AtifSource::User));
        assert!(matches!(trajectory.steps[1].source, AtifSource::Agent));
        assert!(matches!(trajectory.steps[2].source, AtifSource::Agent));
        assert_eq!(trajectory.steps[1].message, "I will inspect.");
        assert_eq!(trajectory.steps[1].tool_calls.as_ref().unwrap().len(), 1);
        assert_eq!(
            trajectory.steps[1]
                .observation
                .as_ref()
                .unwrap()
                .results
                .len(),
            1
        );
        assert_eq!(
            trajectory.steps[2].reasoning_content.as_deref(),
            Some("Done")
        );
        assert_eq!(trajectory.final_metrics.total_steps, 3);
        let encoded = serde_json::to_string(&trajectory).unwrap();
        let decoded: crate::AtifTrajectory = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.tool_call_count(), 1);
        assert_eq!(decoded.observation_count(), 1);
    }

    fn event(seq: u64, kind: &str, payload: &str) -> AgentEvent {
        serde_json::from_str(&format!(
            r#"{{"protocol_version":1,"request_id":"session","seq":{seq},"type":"{kind}","payload":{payload}}}"#
        ))
        .unwrap()
    }

    const TERMINAL_METADATA: &str = r#"{
        "status":"completed",
        "model":"gpt-test",
        "effort":"low",
        "transport":"responses_websocket_v2",
        "orchestration":"local_code_mode",
        "duration_ms":1,
        "duration_ns":30,
        "model_calls":2,
        "steers":0,
        "compactions":0,
        "tool_calls":1,
        "connection_attempts":1,
        "websocket_reconnects":0,
        "response_attempts":2,
        "response_retries":0,
        "connection_duration_ns":1,
        "retry_backoff_duration_ns":0,
        "model_duration_ns":30,
        "warmup_duration_ns":0,
        "tool_work_duration_ns":42,
        "tool_wall_duration_ns":42,
        "usage":{"input_tokens":22,"cached_input_tokens":4,"cache_write_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":0,"total_tokens":27},
        "warmup_usage":{"input_tokens":0,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":0,"reasoning_output_tokens":0,"total_tokens":0},
        "cost_usd":null,
        "cost_status":"not_reported_by_responses_api"
    }"#;
}

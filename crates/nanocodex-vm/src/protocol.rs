use nanocodex_tools::{StandardTool, ToolExecutionWire, ToolInput};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub(crate) enum SessionRequest {
    Tool(ToolRequest),
    WriteFile(WriteFileRequest),
    ReadFile(ReadFileRequest),
    Execute(ExecuteRequest),
    Shutdown(ShutdownRequest),
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub(crate) enum SessionResponse {
    Tool(ToolResponse),
    WriteFile(ControlResponse),
    ReadFile(ReadFileResponse),
    Execute(ExecuteResponse),
    Shutdown(ControlResponse),
}

impl SessionResponse {
    pub const fn id(&self) -> u64 {
        match self {
            Self::Tool(response) => response.id,
            Self::WriteFile(response) | Self::Shutdown(response) => response.id,
            Self::ReadFile(response) => response.id,
            Self::Execute(response) => response.id,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ShutdownRequest {
    pub id: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WriteFileRequest {
    pub id: u64,
    pub path: String,
    pub contents: Vec<u8>,
    pub mode: u32,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadFileRequest {
    pub id: u64,
    pub path: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecuteRequest {
    pub id: u64,
    pub program: String,
    pub arguments: Vec<String>,
    pub current_directory: String,
    pub environment: Vec<(String, String)>,
    pub timeout_millis: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ControlResponse {
    pub id: u64,
    pub error: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadFileResponse {
    pub id: u64,
    pub contents: Option<Vec<u8>>,
    pub error: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecuteResponse {
    pub id: u64,
    pub exit_code: Option<i32>,
    pub stdout: Option<Vec<u8>>,
    pub stderr: Option<Vec<u8>>,
    pub error: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolRequest {
    pub id: u64,
    pub tool: StandardTool,
    pub input: WireToolInput,
    pub context: WireToolContext,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WireToolInput {
    Function { arguments: Box<RawValue> },
    Freeform { input: String },
}

#[cfg(test)]
mod tests {
    use nanocodex_tools::{ToolExecution, ToolInput};
    use serde_json::{json, value::to_raw_value};

    use super::{
        SessionRequest, ShutdownRequest, ToolRequest, ToolResponse, WireToolContext, WireToolInput,
    };

    #[test]
    fn shutdown_request_has_a_stable_typed_shape() {
        let request = SessionRequest::Shutdown(ShutdownRequest { id: 9 });
        let encoded = serde_json::to_string(&request).unwrap();

        assert_eq!(encoded, r#"{"kind":"shutdown","payload":{"id":9}}"#);
    }

    #[test]
    fn function_request_round_trips_opaque_arguments() {
        let request = ToolRequest {
            id: 7,
            tool: nanocodex_tools::StandardTool::ExecCommand,
            input: WireToolInput::from(ToolInput::Function(
                to_raw_value(&json!({"cmd": "pwd"})).unwrap(),
            )),
            context: WireToolContext {
                model: "model".to_owned(),
                session_id: "session".to_owned(),
                call_id: "call".to_owned(),
                output_token_budget: 100,
            },
        };
        let encoded = serde_json::to_string(&request).unwrap();
        let decoded = serde_json::from_str::<ToolRequest>(&encoded).unwrap();
        let ToolInput::Function(arguments) = ToolInput::from(decoded.input) else {
            panic!("function input changed variants");
        };
        assert_eq!(arguments.get(), r#"{"cmd":"pwd"}"#);
    }

    #[test]
    fn execution_response_round_trips_opaque_values() {
        let response = ToolResponse::completed(
            8,
            ToolExecution::from_json(json!({"output": "ok"}), true)
                .into_wire()
                .unwrap(),
        );
        let encoded = serde_json::to_string(&response).unwrap();
        let decoded = serde_json::from_str::<ToolResponse>(&encoded).unwrap();
        assert_eq!(decoded.id, 8);
    }
}

impl From<ToolInput> for WireToolInput {
    fn from(input: ToolInput) -> Self {
        match input {
            ToolInput::Function(arguments) => Self::Function { arguments },
            ToolInput::Freeform(input) => Self::Freeform { input },
        }
    }
}

impl From<WireToolInput> for ToolInput {
    fn from(input: WireToolInput) -> Self {
        match input {
            WireToolInput::Function { arguments } => Self::Function(arguments),
            WireToolInput::Freeform { input } => Self::Freeform(input),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WireToolContext {
    pub model: String,
    pub session_id: String,
    pub call_id: String,
    pub output_token_budget: usize,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolResponse {
    pub id: u64,
    pub execution: Option<ToolExecutionWire>,
    pub error: Option<String>,
}

impl ToolResponse {
    pub const fn completed(id: u64, execution: ToolExecutionWire) -> Self {
        Self {
            id,
            execution: Some(execution),
            error: None,
        }
    }

    pub const fn failed(id: u64, error: String) -> Self {
        Self {
            id,
            execution: None,
            error: Some(error),
        }
    }
}

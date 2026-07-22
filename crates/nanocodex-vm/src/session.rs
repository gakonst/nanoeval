use std::{path::Path, process::Stdio};

use nanocodex_tools::{
    StandardTool, ToolContext, ToolExecution, ToolInput, ToolResult, ToolRuntime,
};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};

use crate::{
    VmToolClient,
    protocol::{ToolRequest, ToolResponse, WireToolContext, WireToolInput},
};

#[derive(Debug, Error)]
pub enum VmToolSessionError {
    #[error("failed to spawn the VMM process: {0}")]
    Spawn(#[source] std::io::Error),

    #[error("the VMM process did not expose piped {0}")]
    MissingPipe(&'static str),

    #[error("VM tool console I/O failed: {0}")]
    Io(#[from] std::io::Error),

    #[error("VM tool protocol JSON failed: {0}")]
    Json(#[from] serde_json::Error),

    #[error("the VM tool console closed before replying")]
    Closed,

    #[error("VM tool response {actual} did not match request {expected}")]
    ResponseId { expected: u64, actual: u64 },

    #[error("guest tool execution failed: {0}")]
    Guest(String),

    #[error("invalid VM tool response: {0}")]
    Protocol(&'static str),
}

/// One persistent VMM child carrying workspace tool calls over its console.
pub struct VmToolSession {
    state: Mutex<SessionState>,
}

struct SessionState {
    _child: Child,
    input: ChildStdin,
    output: Lines<BufReader<ChildStdout>>,
    next_id: u64,
}

impl VmToolSession {
    /// Spawns a VMM command whose guest process runs [`crate::serve_guest`].
    ///
    /// The command's stdin and stdout are reserved for the typed protocol;
    /// stderr remains available for VMM and guest diagnostics.
    ///
    /// # Errors
    ///
    /// Returns an error when the child or either protocol pipe cannot be
    /// created.
    pub fn spawn(command: &mut Command) -> Result<Self, VmToolSessionError> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(VmToolSessionError::Spawn)?;
        let input = child
            .stdin
            .take()
            .ok_or(VmToolSessionError::MissingPipe("stdin"))?;
        let output = child
            .stdout
            .take()
            .ok_or(VmToolSessionError::MissingPipe("stdout"))?;
        Ok(Self {
            state: Mutex::new(SessionState {
                _child: child,
                input,
                output: BufReader::new(output).lines(),
                next_id: 0,
            }),
        })
    }

    async fn request(
        &self,
        tool: StandardTool,
        input: ToolInput,
        context: ToolContext<'_>,
    ) -> Result<ToolExecution, VmToolSessionError> {
        let mut state = self.state.lock().await;
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1);
        let request = ToolRequest {
            id,
            tool,
            input: WireToolInput::from(input),
            context: WireToolContext {
                model: context.model.to_owned(),
                session_id: context.session_id.to_owned(),
                call_id: context.call_id.to_owned(),
                output_token_budget: context.output_token_budget,
            },
        };
        let mut encoded = serde_json::to_vec(&request)?;
        encoded.push(b'\n');
        state.input.write_all(&encoded).await?;
        state.input.flush().await?;

        let line = state
            .output
            .next_line()
            .await?
            .ok_or(VmToolSessionError::Closed)?;
        let response = serde_json::from_str::<ToolResponse>(&line)?;
        if response.id() != id {
            return Err(VmToolSessionError::ResponseId {
                expected: id,
                actual: response.id(),
            });
        }
        match (response.execution, response.error) {
            (Some(execution), None) => ToolExecution::from_wire(execution).map_err(Into::into),
            (None, Some(error)) => Err(VmToolSessionError::Guest(error)),
            _ => Err(VmToolSessionError::Protocol(
                "expected exactly one of execution or error",
            )),
        }
    }
}

#[async_trait::async_trait]
impl VmToolClient for VmToolSession {
    async fn execute(
        &self,
        tool: StandardTool,
        input: ToolInput,
        context: ToolContext<'_>,
    ) -> ToolResult {
        self.request(tool, input, context)
            .await
            .map_err(|error| Box::new(error) as _)
    }
}

pub(crate) async fn serve_guest(workspace: &Path) -> Result<(), VmToolSessionError> {
    let runtime = ToolRuntime::new(workspace, None, None);
    let input = tokio::io::stdin();
    let mut lines = BufReader::new(input).lines();
    let mut output = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let request = serde_json::from_str::<ToolRequest>(&line)?;
        let context = ToolContext {
            model: &request.context.model,
            session_id: &request.context.session_id,
            call_id: &request.context.call_id,
            history: &[],
            output_token_budget: request.context.output_token_budget,
        };
        let execution = runtime
            .execute_tool(request.tool.name(), request.input.into(), context)
            .await;
        let response = match execution.into_wire() {
            Ok(execution) => ToolResponse::completed(request.id, execution),
            Err(error) => ToolResponse::failed(request.id, error.to_string()),
        };
        let mut encoded = serde_json::to_vec(&response)?;
        encoded.push(b'\n');
        output.write_all(&encoded).await?;
        output.flush().await?;
    }
    Ok(())
}

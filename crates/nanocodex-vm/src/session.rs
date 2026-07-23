use std::{
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use nanocodex_tools::{
    StandardTool, ToolContext, ToolExecution, ToolInput, ToolResult, ToolRuntime,
};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};
use tracing::{Instrument, info, info_span};

use crate::{
    VmToolClient,
    protocol::{
        ControlResponse, ExecuteRequest, ExecuteResponse, ReadFileRequest, ReadFileResponse,
        SessionRequest, SessionResponse, ShutdownRequest, ToolRequest, ToolResponse,
        WireToolContext, WireToolInput, WriteFileRequest,
    },
};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// One trusted command executed by the evaluation harness inside the guest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VmCommand {
    program: String,
    arguments: Vec<String>,
    current_directory: String,
    environment: Vec<(String, String)>,
    timeout: Duration,
}

impl VmCommand {
    #[must_use]
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            arguments: Vec::new(),
            current_directory: "/".to_owned(),
            environment: Vec::new(),
            timeout: Duration::from_secs(60),
        }
    }

    #[must_use]
    pub fn arg(mut self, argument: impl Into<String>) -> Self {
        self.arguments.push(argument.into());
        self
    }

    #[must_use]
    pub fn current_directory(mut self, directory: impl Into<String>) -> Self {
        self.current_directory = directory.into();
        self
    }

    #[must_use]
    pub fn environment(mut self, environment: impl IntoIterator<Item = (String, String)>) -> Self {
        self.environment.extend(environment);
        self
    }

    #[must_use]
    pub const fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Complete output from one trusted harness command in the guest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VmCommandOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

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

    #[error("the VMM did not exit within {0:?} after guest shutdown")]
    ShutdownTimeout(Duration),

    #[error("the VMM exited unsuccessfully after guest shutdown: {0}")]
    VmmExit(ExitStatus),
}

/// One persistent VMM child carrying workspace tool calls over its console.
#[derive(Clone)]
pub struct VmToolSession {
    inner: Arc<VmToolSessionInner>,
}

struct VmToolSessionInner {
    spawned_at: Instant,
    state: Mutex<SessionState>,
}

struct SessionState {
    child: Child,
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
        let program = command
            .as_std()
            .get_program()
            .to_string_lossy()
            .into_owned();
        let command_content = format!("{:?}", command.as_std());
        let argument_count = command.as_std().get_args().count();
        let span = info_span!(
            target: "nanocodex_vm",
            "vm.session.spawn",
            otel.kind = "internal",
            otel.status_code = tracing::field::Empty,
            process.executable.name = program.as_str(),
            process.command_args.count = argument_count,
            process.id = tracing::field::Empty,
            status = tracing::field::Empty,
            error.message = tracing::field::Empty,
            duration_ns = tracing::field::Empty,
        );
        record_vm_content(&span, "vm.command", &command_content);
        let started_at = Instant::now();
        let result = span.in_scope(|| {
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .map_err(VmToolSessionError::Spawn)?;
            if let Some(process_id) = child.id() {
                span.record("process.id", process_id);
            }
            let input = child
                .stdin
                .take()
                .ok_or(VmToolSessionError::MissingPipe("stdin"))?;
            let output = child
                .stdout
                .take()
                .ok_or(VmToolSessionError::MissingPipe("stdout"))?;
            Ok(Self {
                inner: Arc::new(VmToolSessionInner {
                    spawned_at: Instant::now(),
                    state: Mutex::new(SessionState {
                        child,
                        input,
                        output: BufReader::new(output).lines(),
                        next_id: 0,
                    }),
                }),
            })
        });
        record_vm_result(&span, started_at, &result);
        result
    }

    async fn request(
        &self,
        tool: StandardTool,
        input: ToolInput,
        context: ToolContext<'_>,
    ) -> Result<ToolExecution, VmToolSessionError> {
        let (input_kind, input_bytes) = match &input {
            ToolInput::Function(arguments) => ("function", arguments.get().len()),
            ToolInput::Freeform(input) => ("freeform", input.len()),
        };
        let span = info_span!(
            target: "nanocodex_vm",
            "vm.tool.rpc",
            otel.kind = "client",
            otel.status_code = tracing::field::Empty,
            rpc.system = "libkrun.console",
            rpc.method = tool.name(),
            tool.name = tool.name(),
            session.id = context.session_id,
            tool.call_id = context.call_id,
            tool.input.kind = input_kind,
            tool.input.bytes = input_bytes,
            rpc.request.id = tracing::field::Empty,
            rpc.request.bytes = tracing::field::Empty,
            rpc.response.bytes = tracing::field::Empty,
            rpc.queue.duration_ns = tracing::field::Empty,
            vm.session.first_call = tracing::field::Empty,
            vm.session.age_ns = tracing::field::Empty,
            tool.success = tracing::field::Empty,
            status = tracing::field::Empty,
            error.message = tracing::field::Empty,
            duration_ns = tracing::field::Empty,
        );
        let started_at = Instant::now();
        let result = self
            .request_inner(tool, input, context, &span)
            .instrument(span.clone())
            .await;
        if let Ok(execution) = &result {
            span.record("tool.success", execution.success);
        }
        record_vm_result(&span, started_at, &result);
        result
    }

    async fn request_inner(
        &self,
        tool: StandardTool,
        input: ToolInput,
        context: ToolContext<'_>,
        span: &tracing::Span,
    ) -> Result<ToolExecution, VmToolSessionError> {
        let queued_at = Instant::now();
        let mut state = self.inner.state.lock().await;
        span.record("rpc.queue.duration_ns", elapsed_ns(queued_at));
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1);
        span.record("rpc.request.id", id);
        span.record("vm.session.first_call", id == 0);
        let request = SessionRequest::Tool(ToolRequest {
            id,
            tool,
            input: WireToolInput::from(input),
            context: WireToolContext {
                model: context.model.to_owned(),
                session_id: context.session_id.to_owned(),
                call_id: context.call_id.to_owned(),
                output_token_budget: context.output_token_budget,
            },
        });
        let encoded = serde_json::to_string(&request)?;
        span.record("rpc.request.bytes", encoded.len());
        record_vm_content(span, "tool.request", &encoded);
        state.input.write_all(encoded.as_bytes()).await?;
        state.input.write_all(b"\n").await?;
        state.input.flush().await?;

        let line = state
            .output
            .next_line()
            .await?
            .ok_or(VmToolSessionError::Closed)?;
        span.record("rpc.response.bytes", line.len());
        span.record("vm.session.age_ns", elapsed_ns(self.inner.spawned_at));
        record_vm_content(span, "tool.response", &line);
        let response = serde_json::from_str::<SessionResponse>(&line)?;
        if response.id() != id {
            return Err(VmToolSessionError::ResponseId {
                expected: id,
                actual: response.id(),
            });
        }
        let SessionResponse::Tool(response) = response else {
            return Err(VmToolSessionError::Protocol("expected a tool response"));
        };
        match (response.execution, response.error) {
            (Some(execution), None) => ToolExecution::from_wire(execution).map_err(Into::into),
            (None, Some(error)) => Err(VmToolSessionError::Guest(error)),
            _ => Err(VmToolSessionError::Protocol(
                "expected exactly one of execution or error",
            )),
        }
    }

    /// Writes one harness-owned file into the guest after the agent phase.
    ///
    /// # Errors
    ///
    /// Returns an error when the console closes, file creation fails in the
    /// guest, or the typed response is invalid.
    pub async fn write_file(
        &self,
        path: impl Into<String>,
        contents: Vec<u8>,
        mode: u32,
    ) -> Result<(), VmToolSessionError> {
        let response = self
            .control_request(|id| {
                SessionRequest::WriteFile(WriteFileRequest {
                    id,
                    path: path.into(),
                    contents,
                    mode,
                })
            })
            .await?;
        let SessionResponse::WriteFile(response) = response else {
            return Err(VmToolSessionError::Protocol(
                "expected a write-file response",
            ));
        };
        control_result(response)
    }

    /// Reads one result artifact from the guest.
    ///
    /// # Errors
    ///
    /// Returns an error when the console closes, the file cannot be read, or
    /// the typed response is invalid.
    pub async fn read_file(&self, path: impl Into<String>) -> Result<Vec<u8>, VmToolSessionError> {
        let response = self
            .control_request(|id| {
                SessionRequest::ReadFile(ReadFileRequest {
                    id,
                    path: path.into(),
                })
            })
            .await?;
        let SessionResponse::ReadFile(ReadFileResponse {
            contents, error, ..
        }) = response
        else {
            return Err(VmToolSessionError::Protocol(
                "expected a read-file response",
            ));
        };
        match (contents, error) {
            (Some(contents), None) => Ok(contents),
            (None, Some(error)) => Err(VmToolSessionError::Guest(error)),
            _ => Err(VmToolSessionError::Protocol(
                "expected exactly one of contents or error",
            )),
        }
    }

    /// Executes a trusted evaluation-harness command in the guest.
    ///
    /// # Errors
    ///
    /// Returns an error when the console closes, the command cannot run or
    /// exceeds its deadline, or the typed response is invalid.
    pub async fn command(&self, command: VmCommand) -> Result<VmCommandOutput, VmToolSessionError> {
        let timeout_millis = u64::try_from(command.timeout.as_millis()).unwrap_or(u64::MAX);
        let response = self
            .control_request(|id| {
                SessionRequest::Execute(ExecuteRequest {
                    id,
                    program: command.program,
                    arguments: command.arguments,
                    current_directory: command.current_directory,
                    environment: command.environment,
                    timeout_millis,
                })
            })
            .await?;
        let SessionResponse::Execute(ExecuteResponse {
            exit_code,
            stdout,
            stderr,
            error,
            ..
        }) = response
        else {
            return Err(VmToolSessionError::Protocol("expected an execute response"));
        };
        match (exit_code, stdout, stderr, error) {
            (Some(exit_code), Some(stdout), Some(stderr), None) => Ok(VmCommandOutput {
                exit_code,
                stdout,
                stderr,
            }),
            (None, None, None, Some(error)) => Err(VmToolSessionError::Guest(error)),
            _ => Err(VmToolSessionError::Protocol(
                "invalid execute response fields",
            )),
        }
    }

    /// Flushes guest filesystems and waits for the VMM process to exit.
    ///
    /// No tool or control request may be sent through this session after a
    /// successful shutdown.
    ///
    /// # Errors
    ///
    /// Returns an error when the guest cannot acknowledge the request, the
    /// VMM does not stop promptly, or it exits unsuccessfully.
    pub async fn shutdown(&self) -> Result<(), VmToolSessionError> {
        let response = self
            .control_request(|id| SessionRequest::Shutdown(ShutdownRequest { id }))
            .await?;
        let SessionResponse::Shutdown(response) = response else {
            return Err(VmToolSessionError::Protocol("expected a shutdown response"));
        };
        control_result(response)?;

        let mut state = self.inner.state.lock().await;
        let status = tokio::time::timeout(SHUTDOWN_TIMEOUT, state.child.wait())
            .await
            .map_err(|_| VmToolSessionError::ShutdownTimeout(SHUTDOWN_TIMEOUT))??;
        if !status.success() {
            return Err(VmToolSessionError::VmmExit(status));
        }
        Ok(())
    }

    async fn control_request(
        &self,
        make_request: impl FnOnce(u64) -> SessionRequest,
    ) -> Result<SessionResponse, VmToolSessionError> {
        let mut state = self.inner.state.lock().await;
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1);
        let encoded = serde_json::to_string(&make_request(id))?;
        state.input.write_all(encoded.as_bytes()).await?;
        state.input.write_all(b"\n").await?;
        state.input.flush().await?;
        let line = state
            .output
            .next_line()
            .await?
            .ok_or(VmToolSessionError::Closed)?;
        let response = serde_json::from_str::<SessionResponse>(&line)?;
        if response.id() != id {
            return Err(VmToolSessionError::ResponseId {
                expected: id,
                actual: response.id(),
            });
        }
        Ok(response)
    }
}

fn control_result(response: ControlResponse) -> Result<(), VmToolSessionError> {
    match response.error {
        None => Ok(()),
        Some(error) => Err(VmToolSessionError::Guest(error)),
    }
}

fn record_vm_result<T, E>(span: &tracing::Span, started_at: Instant, result: &Result<T, E>)
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
                    target: "nanocodex_vm",
                    duration_ns,
                    status = "completed",
                    "VM operation completed"
                );
            });
        }
        Err(error) => {
            span.record("status", "failed");
            span.record("otel.status_code", "ERROR");
            span.record("error.message", tracing::field::display(error));
            span.in_scope(|| {
                info!(
                    target: "nanocodex_vm",
                    duration_ns,
                    status = "failed",
                    error = %error,
                    "VM operation failed"
                );
            });
        }
    }
}

fn record_vm_content(span: &tracing::Span, kind: &'static str, content: &str) {
    span.in_scope(|| {
        info!(
            target: "nanocodex_vm",
            content_kind = kind,
            content,
            "VM tool content"
        );
    });
}

fn elapsed_ns(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_nanos()).unwrap_or(u64::MAX)
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
        let request = serde_json::from_str::<SessionRequest>(&line)?;
        let (response, shutdown) = match request {
            SessionRequest::Tool(request) => {
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
                (
                    SessionResponse::Tool(match execution.into_wire() {
                        Ok(execution) => ToolResponse::completed(request.id, execution),
                        Err(error) => ToolResponse::failed(request.id, error.to_string()),
                    }),
                    false,
                )
            }
            SessionRequest::WriteFile(request) => (
                SessionResponse::WriteFile(write_guest_file(request).await),
                false,
            ),
            SessionRequest::ReadFile(request) => (
                SessionResponse::ReadFile(read_guest_file(request).await),
                false,
            ),
            SessionRequest::Execute(request) => (
                SessionResponse::Execute(execute_guest_command(request).await),
                false,
            ),
            SessionRequest::Shutdown(request) => {
                let response = sync_guest_filesystems(request).await;
                let shutdown = response.error.is_none();
                (SessionResponse::Shutdown(response), shutdown)
            }
        };
        let mut encoded = serde_json::to_vec(&response)?;
        encoded.push(b'\n');
        output.write_all(&encoded).await?;
        output.flush().await?;
        if shutdown {
            return Ok(());
        }
    }
    Ok(())
}

async fn sync_guest_filesystems(request: ShutdownRequest) -> ControlResponse {
    let error = match Command::new("/bin/sync").status().await {
        Ok(status) if status.success() => None,
        Ok(status) => Some(format!("sync exited with {status}")),
        Err(error) => Some(error.to_string()),
    };
    ControlResponse {
        id: request.id,
        error,
    }
}

async fn write_guest_file(request: WriteFileRequest) -> ControlResponse {
    let result = async {
        let path = PathBuf::from(&request.path);
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("file path has no parent"))?;
        tokio::fs::create_dir_all(parent).await?;
        tokio::fs::write(&path, request.contents).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(request.mode)).await?;
        }
        Ok::<_, std::io::Error>(())
    }
    .await;
    ControlResponse {
        id: request.id,
        error: result.err().map(|error| error.to_string()),
    }
}

async fn read_guest_file(request: ReadFileRequest) -> ReadFileResponse {
    match tokio::fs::read(&request.path).await {
        Ok(contents) => ReadFileResponse {
            id: request.id,
            contents: Some(contents),
            error: None,
        },
        Err(error) => ReadFileResponse {
            id: request.id,
            contents: None,
            error: Some(error.to_string()),
        },
    }
}

async fn execute_guest_command(request: ExecuteRequest) -> ExecuteResponse {
    let mut command = Command::new(&request.program);
    command
        .args(&request.arguments)
        .current_dir(&request.current_directory)
        .env_clear()
        .envs(request.environment.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let timeout = Duration::from_millis(request.timeout_millis);
    match tokio::time::timeout(timeout, command.output()).await {
        Ok(Ok(output)) => ExecuteResponse {
            id: request.id,
            exit_code: Some(output.status.code().unwrap_or(1)),
            stdout: Some(output.stdout),
            stderr: Some(output.stderr),
            error: None,
        },
        Ok(Err(error)) => ExecuteResponse {
            id: request.id,
            exit_code: None,
            stdout: None,
            stderr: None,
            error: Some(error.to_string()),
        },
        Err(_) => ExecuteResponse {
            id: request.id,
            exit_code: None,
            stdout: None,
            stderr: None,
            error: Some(format!("guest command exceeded {timeout:?}")),
        },
    }
}

#[cfg(test)]
mod tracing_tests {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    use nanocodex_tools::{StandardTool, ToolContext, ToolInput};
    use serde_json::{json, value::to_raw_value};
    use tracing::{Id, Instrument, Subscriber, field::Visit, span::Attributes};
    use tracing_subscriber::{
        Layer, layer::Context as LayerContext, prelude::*, registry::LookupSpan,
    };

    use super::VmToolSession;

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
    fn vm_rpc_is_timed_and_parented_to_the_calling_tool() {
        let response = r#"{"kind":"tool","payload":{"id":0,"execution":{"output":"ok","success":true,"code_mode_value":null,"metadata":null,"process_trace":null},"error":null}}"#;
        let script = format!("IFS= read -r request\nprintf '%s\\n' '{response}'");
        let mut command = tokio::process::Command::new("/bin/sh");
        command.arg("-c").arg(script);
        let capture = TraceCapture::default();
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(capture.clone()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        tracing::dispatcher::with_default(&dispatch, || {
            runtime.block_on(async {
                let session = VmToolSession::spawn(&mut command).unwrap();
                let context = ToolContext {
                    model: "test-model",
                    session_id: "test-session",
                    call_id: "test-call",
                    history: &[],
                    output_token_budget: 1_000,
                };
                let execution = session
                    .request(
                        StandardTool::ExecCommand,
                        ToolInput::Function(to_raw_value(&json!({"cmd": "true"})).unwrap()),
                        context,
                    )
                    .instrument(tracing::info_span!("test.tool.execute"))
                    .await
                    .unwrap();
                assert!(execution.success);
            });
        });

        let spans = capture.0.lock().unwrap();
        let (tool_id, _) = spans
            .iter()
            .find(|(_, span)| span.name == "test.tool.execute")
            .unwrap();
        let rpc = spans
            .values()
            .find(|span| span.name == "vm.tool.rpc")
            .unwrap();
        assert_eq!(rpc.parent, Some(*tool_id));
        assert_eq!(
            rpc.fields.get("status").map(String::as_str),
            Some("completed")
        );
        assert_eq!(
            rpc.fields.get("vm.session.first_call").map(String::as_str),
            Some("true")
        );
        assert!(rpc.fields.contains_key("rpc.queue.duration_ns"));
        assert!(rpc.fields.contains_key("duration_ns"));
        assert!(spans.values().any(|span| span.name == "vm.session.spawn"));
    }
}

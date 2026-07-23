mod protocol;
mod session;

use std::{path::Path, sync::Arc};

use nanocodex_tools::{StandardTool, Tool, ToolContext, ToolDefinition, ToolInput, ToolResult};

pub use session::{VmCommand, VmCommandOutput, VmToolSession, VmToolSessionError};

/// One VM-aware execution capability shared by all proxied workspace tools.
///
/// The concrete client owns transport, guest session routing, cancellation,
/// and conversion of the guest's typed result into Nanocodex's `ToolResult`.
#[async_trait::async_trait]
pub trait VmToolClient: Send + Sync {
    async fn execute(
        &self,
        tool: StandardTool,
        input: ToolInput,
        context: ToolContext<'_>,
    ) -> ToolResult;
}

/// Clone-cheap factory for the standard tools whose effects belong in a VM.
#[derive(Clone)]
pub struct VmTools {
    client: Arc<dyn VmToolClient>,
}

impl VmTools {
    #[must_use]
    pub fn new(client: impl VmToolClient + 'static) -> Self {
        Self {
            client: Arc::new(client),
        }
    }

    #[must_use]
    pub fn exec_command_tool(&self) -> VmTool {
        self.tool(StandardTool::ExecCommand)
    }

    #[must_use]
    pub fn write_stdin_tool(&self) -> VmTool {
        self.tool(StandardTool::WriteStdin)
    }

    #[must_use]
    pub fn apply_patch_tool(&self) -> VmTool {
        self.tool(StandardTool::ApplyPatch)
    }

    #[must_use]
    pub fn view_image_tool(&self) -> VmTool {
        self.tool(StandardTool::ViewImage)
    }

    fn tool(&self, standard: StandardTool) -> VmTool {
        VmTool {
            standard,
            client: Arc::clone(&self.client),
        }
    }
}

/// One standard Nanocodex tool whose execution is forwarded into a VM.
#[derive(Clone)]
pub struct VmTool {
    standard: StandardTool,
    client: Arc<dyn VmToolClient>,
}

impl VmTool {
    #[must_use]
    pub const fn standard(&self) -> StandardTool {
        self.standard
    }
}

#[async_trait::async_trait]
impl Tool for VmTool {
    fn name(&self) -> &'static str {
        self.standard.name()
    }

    fn definition(&self) -> ToolDefinition {
        self.standard.definition()
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        self.client.execute(self.standard, input, context).await
    }
}

/// Serves canonical workspace-tool requests over the guest's stdin/stdout.
///
/// A single invocation retains the native `ToolRuntime`, including interactive
/// shell sessions, until the input stream closes.
///
/// # Errors
///
/// Returns an error for malformed protocol messages or guest console I/O.
pub async fn serve_guest(workspace: impl AsRef<Path>) -> Result<(), VmToolSessionError> {
    session::serve_guest(workspace.as_ref()).await
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use nanocodex_tools::{
        StandardTool, Tool, ToolContext, ToolExecution, ToolInput, Tools, UpdatePlanTool,
    };

    use super::{VmToolClient, VmTools};

    #[derive(Default)]
    struct RecordingClient {
        calls: Mutex<Vec<StandardTool>>,
    }

    #[async_trait::async_trait]
    impl VmToolClient for RecordingClient {
        async fn execute(
            &self,
            tool: StandardTool,
            _input: ToolInput,
            _context: ToolContext<'_>,
        ) -> nanocodex_tools::ToolResult {
            self.calls.lock().unwrap().push(tool);
            Ok(ToolExecution::text(tool.name()))
        }
    }

    #[test]
    fn composes_vm_workspace_tools_with_the_host_plan_tool() {
        let vm = VmTools::new(RecordingClient::default());
        let tools = Tools::builder()
            .without_defaults()
            .tool(vm.exec_command_tool())
            .tool(vm.write_stdin_tool())
            .tool(vm.apply_patch_tool())
            .tool(vm.view_image_tool())
            .tool(UpdatePlanTool::new())
            .build()
            .unwrap();

        assert!(!tools.workspace_enabled());
    }

    #[test]
    fn definitions_are_the_upstream_standard_contracts() {
        let vm = VmTools::new(RecordingClient::default());
        for (tool, standard) in [
            (vm.exec_command_tool(), StandardTool::ExecCommand),
            (vm.write_stdin_tool(), StandardTool::WriteStdin),
            (vm.apply_patch_tool(), StandardTool::ApplyPatch),
            (vm.view_image_tool(), StandardTool::ViewImage),
        ] {
            assert_eq!(tool.name(), standard.name());
            assert_eq!(
                serde_json::to_value(tool.definition()).unwrap(),
                serde_json::to_value(standard.definition()).unwrap()
            );
        }
    }
}

use std::{error::Error, path::PathBuf};

use nanocodex::{Tool, ToolContext, ToolExecution, ToolInput, ToolOutputBody, ToolOutputContent};
use nanocodex_vm::{VmToolSession, VmTools};
use nanovm::{GuestCommand, KrunVm, Network, VmConfig};
use serde::Deserialize;
use serde_json::value::to_raw_value;
use tokio::process::Command;

const GUEST_RUNTIME: &str = "/usr/local/bin/nanocodex-vm-guest";
type AnyError = Box<dyn Error + Send + Sync>;

#[derive(Deserialize)]
struct CommandOutput {
    output: String,
    exit_code: Option<i32>,
    session_id: Option<i64>,
}

#[tokio::main]
#[allow(
    clippy::too_many_lines,
    reason = "the executable intentionally presents one linear end-to-end VM tool proof"
)]
async fn main() -> Result<(), AnyError> {
    let mut arguments = std::env::args_os().skip(1);
    if arguments.next().as_deref() == Some(std::ffi::OsStr::new("--vmm")) {
        let root = arguments
            .next()
            .map(PathBuf::from)
            .ok_or("VMM mode requires a rootfs path")?;
        let config = VmConfig::new(root)
            .cpus(2)
            .memory_mib(768)
            .network(Network::Disabled);
        let command = GuestCommand::new(GUEST_RUNTIME).arg("/workspace");
        KrunVm::new(&config)?.run(&command)?;
        return Ok(());
    }

    let root = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: vm-tools ROOTFS")?;
    let executable = std::env::current_exe()?;
    let mut vmm = Command::new(executable);
    vmm.arg("--vmm").arg(root);
    let session = VmToolSession::spawn(&mut vmm)?;
    let vm = VmTools::new(session);
    let context = ToolContext {
        model: "vm-proof",
        session_id: "session-1",
        call_id: "call-1",
        history: &[],
        output_token_budget: 10_000,
    };

    let execution = vm
        .exec_command_tool()
        .execute(
            function_input(&serde_json::json!({
                "cmd": "printf 'kernel='; uname -srm; printf 'pid1='; cat /proc/1/comm",
                "workdir": "/workspace",
                "login": false
            }))?,
            context,
        )
        .await?;
    let output = command_output(execution)?;
    println!("exec_command: {}", output.output.trim());
    if output.exit_code != Some(0) {
        return Err("exec_command did not exit successfully".into());
    }

    let execution = vm
        .exec_command_tool()
        .execute(
            function_input(&serde_json::json!({
                "cmd": "cat",
                "workdir": "/workspace",
                "login": false,
                "yield_time_ms": 250
            }))?,
            context,
        )
        .await?;
    let output = command_output(execution)?;
    let shell_session = output
        .session_id
        .ok_or("exec_command did not retain an interactive session")?;
    println!("exec_command session: {shell_session}");

    let execution = vm
        .write_stdin_tool()
        .execute(
            function_input(&serde_json::json!({
                "session_id": shell_session,
                "chars": "from-host\n",
                "yield_time_ms": 1_000
            }))?,
            context,
        )
        .await?;
    let mut output = command_output(execution)?;
    for _ in 0..3 {
        if output.output.contains("from-host") {
            break;
        }
        output = command_output(
            vm.write_stdin_tool()
                .execute(
                    function_input(&serde_json::json!({
                        "session_id": shell_session,
                        "yield_time_ms": 1_000
                    }))?,
                    context,
                )
                .await?,
        )?;
    }
    println!("write_stdin: {}", output.output.trim());
    if !output.output.contains("from-host") {
        return Err("write_stdin did not reach the retained guest process".into());
    }

    let proof_file = format!("vm-proof-{}.txt", std::process::id());
    let patch = format!(
        "*** Begin Patch\n*** Add File: {proof_file}\n+changed inside the guest\n*** End Patch"
    );
    let execution = vm
        .apply_patch_tool()
        .execute(ToolInput::Freeform(patch), context)
        .await?;
    println!("apply_patch: {}", text_output(execution)?.trim());

    let execution = vm
        .exec_command_tool()
        .execute(
            function_input(&serde_json::json!({
                "cmd": "printf iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII= | base64 -d > pixel.png",
                "workdir": "/workspace",
                "login": false
            }))?,
            context,
        )
        .await?;
    if command_output(execution)?.exit_code != Some(0) {
        return Err("failed to prepare guest image fixture".into());
    }

    let execution = vm
        .view_image_tool()
        .execute(
            function_input(&serde_json::json!({
                "path": "pixel.png",
                "detail": "original"
            }))?,
            context,
        )
        .await?;
    let ToolOutputBody::Content(image_items) = execution.output else {
        return Err("view_image did not return multimodal content".into());
    };
    let Some(ToolOutputContent::InputImage { image_url, detail }) = image_items.into_iter().next()
    else {
        return Err("view_image did not return an image".into());
    };
    println!(
        "view_image: detail={detail:?}, data_url_bytes={}",
        image_url.len()
    );
    println!("all VM-owned tools executed through one retained libkrun VM");
    Ok(())
}

fn function_input(value: &serde_json::Value) -> Result<ToolInput, serde_json::Error> {
    to_raw_value(value).map(ToolInput::Function)
}

fn command_output(execution: ToolExecution) -> Result<CommandOutput, AnyError> {
    let text = text_output(execution)?;
    serde_json::from_str(&text).map_err(Into::into)
}

fn text_output(execution: ToolExecution) -> Result<String, AnyError> {
    if !execution.success {
        return Err("tool execution reported failure".into());
    }
    match execution.output {
        ToolOutputBody::Text(text) => Ok(text),
        ToolOutputBody::Content(_) => Err("expected text tool output".into()),
    }
}

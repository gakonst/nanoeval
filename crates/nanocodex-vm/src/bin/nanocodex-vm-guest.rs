use std::path::PathBuf;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), nanocodex_vm::VmToolSessionError> {
    let workspace = std::env::args_os()
        .nth(1)
        .map_or_else(|| PathBuf::from("/workspace"), PathBuf::from);
    nanocodex_vm::serve_guest(workspace).await
}

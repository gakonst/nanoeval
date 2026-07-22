use std::{error::Error, ffi::OsString, path::PathBuf};

use nanovm::{GuestCommand, KrunVm, VmConfig};

fn main() -> Result<(), Box<dyn Error>> {
    let root = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: cargo run -p nanoeval-examples --bin run-vm -- ROOT COMMAND [ARGS...]")?;
    let arguments = std::env::args_os().skip(2).collect::<Vec<OsString>>();
    let (program, arguments) = arguments
        .split_first()
        .ok_or("guest command must not be empty")?;
    let config = VmConfig::new(root);
    let command = GuestCommand::new(program).args(arguments);

    KrunVm::new(&config)?.run(&command)?;
    Ok(())
}

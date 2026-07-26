use std::{error::Error, io, path::PathBuf, time::Instant};

use clap::Parser;
use nanovm_browser::BrowserVmBuilder;

type AnyError = Box<dyn Error + Send + Sync>;

#[derive(Debug, Parser)]
#[command(about = "Start one private headed Chromium microVM")]
struct Args {
    /// Prepared immutable browser root disk.
    #[arg(long)]
    root_disk: PathBuf,

    /// Signed Nanoeval binary exposing `vm run`.
    #[arg(long)]
    vmm: PathBuf,

    /// Gvproxy executable used for the private network and CDP forward.
    #[arg(long)]
    gvproxy: PathBuf,

    /// Directory containing the libkrun firmware runtime.
    #[arg(long)]
    firmware_directory: Option<PathBuf>,

    /// Guest vCPU count.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u8).range(1..))]
    cpus: u8,

    /// Guest memory in MiB.
    #[arg(long, default_value_t = 2_048, value_parser = clap::value_parser!(u32).range(1..))]
    memory_mib: u32,
}

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    let args = Args::parse();
    let mut browser = BrowserVmBuilder::new(args.root_disk, args.vmm, args.gvproxy)
        .cpus(args.cpus)
        .memory_mib(args.memory_mib);
    if let Some(firmware) = args.firmware_directory {
        browser = browser.firmware_directory(firmware);
    }
    let started_at = Instant::now();
    let browser = browser.spawn().await?;
    println!("{}", browser.cdp_endpoint());
    eprintln!(
        "Headed Chromium became ready in {:.3}s. Press Enter to stop it.",
        started_at.elapsed().as_secs_f64()
    );
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        io::stdin().read_line(&mut line)
    })
    .await??;
    browser.shutdown().await?;
    Ok(())
}

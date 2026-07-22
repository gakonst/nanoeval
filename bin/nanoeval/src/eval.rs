use std::{io, path::PathBuf};

use clap::Args;
use eyre::Result;
use nanoeval::{EvalResult, Nanoeval, Task};

use crate::config::AgentArgs;

#[derive(Args)]
pub(crate) struct Eval {
    /// Terminal-Bench task directory.
    task: PathBuf,

    /// Parent directory for the retained Harbor-compatible job.
    #[arg(long, default_value = "nanoeval-runs")]
    output: PathBuf,

    /// Number of fresh, independent attempts.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..))]
    trials: u16,

    /// Maximum number of attempts executing at once.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..))]
    concurrency: u16,

    /// Print typed results as JSON instead of a human summary.
    #[arg(long)]
    json: bool,

    #[command(flatten)]
    agent: AgentArgs,
}

impl Eval {
    pub(crate) async fn run(self) -> Result<()> {
        let task = Task::load(&self.task)?;
        let (eval, _events) = Nanoeval::builder(self.agent.builder()?)
            .output_directory(self.output)
            .max_concurrency(usize::from(self.concurrency))
            .build()?;
        let results = eval.task_n(task, usize::from(self.trials)).await?;
        if self.json {
            serde_json::to_writer_pretty(io::stdout().lock(), &results)?;
            println!();
        } else {
            Self::write_summary(&eval, &results);
        }
        Ok(())
    }

    fn write_summary(eval: &Nanoeval, results: &[EvalResult]) {
        for result in results {
            println!("{}: {:?}", result.trial_name, result.status);
        }
        println!("Harbor job: {}", eval.output_directory().display());
    }
}

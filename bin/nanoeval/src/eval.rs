use std::{io, path::PathBuf};

use clap::Args;
use eyre::{Result, eyre};
use nanoeval::{EvalEventKind, EvalResult, Nanoeval, NanoevalEventStream, Task};
use nanoeval_harbor::{Harbor, HarborJob};

use crate::config::AgentArgs;

#[derive(Args)]
pub(crate) struct Eval {
    /// Terminal-Bench task directory. Repeat for multiple evals in one job.
    #[arg(long = "task", required = true, value_name = "DIRECTORY")]
    tasks: Vec<PathBuf>,

    /// Parent directory for the retained Harbor-compatible job.
    #[arg(long, default_value = "nanoeval-runs")]
    output: PathBuf,

    /// Number of fresh, independent attempts per task.
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
        let trials = usize::from(self.trials);
        let tasks = self
            .tasks
            .into_iter()
            .map(Task::load)
            .collect::<Result<Vec<_>, _>>()?;
        let attempt_count = tasks.len() * trials;
        let attempts = tasks
            .into_iter()
            .flat_map(|task| std::iter::repeat_n(task, trials));
        let (eval, events) = Nanoeval::builder(self.agent.builder()?)
            .output_directory(self.output)
            .max_concurrency(usize::from(self.concurrency))
            .build()?;
        let harbor = Harbor::new(&eval)?.record(events.subscribe())?;
        let progress = tokio::spawn(report_progress(events.subscribe(), attempt_count));
        let results = eval.tasks(attempts).await?;
        let job = harbor.finish(results.clone()).await?;
        progress.await??;
        if self.json {
            serde_json::to_writer_pretty(io::stdout().lock(), &results)?;
            println!();
        } else {
            Self::write_summary(&job, &results);
        }
        Ok(())
    }

    fn write_summary(job: &HarborJob, results: &[EvalResult]) {
        for result in results {
            println!("{}: {:?}", result.trial_name, result.status);
        }
        println!("Harbor job: {}", job.directory().display());
    }
}

async fn report_progress(mut events: NanoevalEventStream, expected: usize) -> Result<()> {
    let mut completed = 0;
    while completed < expected {
        let event = events
            .recv()
            .await?
            .ok_or_else(|| eyre!("event stream closed after {completed} of {expected} attempts"))?;
        match &event.kind {
            EvalEventKind::AttemptStarted { .. } => {
                eprintln!("{}: started", event.trial_name);
            }
            EvalEventKind::Completed(result) => {
                completed += 1;
                eprintln!("{}: {:?}", event.trial_name, result.status);
            }
            EvalEventKind::Agent(_)
            | EvalEventKind::VerifierStarted
            | EvalEventKind::VerifierOutput { .. }
            | EvalEventKind::VerifierCompleted(_) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::Parser;

    use super::Eval;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        eval: Eval,
    }

    #[test]
    fn accepts_repeated_tasks_with_per_task_trials() {
        let cli = TestCli::try_parse_from([
            "nanoeval",
            "--task",
            "tasks/first",
            "--task",
            "tasks/second",
            "--trials",
            "5",
            "--concurrency",
            "10",
        ])
        .unwrap();

        assert_eq!(
            cli.eval.tasks,
            [PathBuf::from("tasks/first"), PathBuf::from("tasks/second")]
        );
        assert_eq!(cli.eval.trials, 5);
        assert_eq!(cli.eval.concurrency, 10);
    }

    #[test]
    fn requires_at_least_one_task() {
        let Err(error) = TestCli::try_parse_from(["nanoeval"]) else {
            panic!("a task should be required");
        };
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }
}

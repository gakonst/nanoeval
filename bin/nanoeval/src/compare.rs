use std::{
    io::{self, Write},
    path::PathBuf,
};

use clap::Args;
use eyre::Result;
use nanoeval_harbor::{PublishedQuery, PublishedResults, PublishedTask, PublishedTrial};

#[derive(Args)]
pub(crate) struct Compare {
    /// Terminal-Bench task name, with or without the terminal-bench/ prefix.
    #[arg(value_name = "TASK")]
    task: String,

    /// Prefer and identify attempts for this exact Harbor task checksum.
    #[arg(long, value_name = "SHA256")]
    checksum: Option<String>,

    /// Return at most one successful attempt from this many harness submissions.
    #[arg(long, default_value_t = 10)]
    limit: usize,

    /// Match a harness, agent, or model name. Repeat to match any value.
    #[arg(long, value_name = "TEXT")]
    agent: Vec<String>,

    /// Content-addressed cache for the public archive index and downloaded artifacts.
    #[arg(long, default_value = ".cache/nanoeval/published")]
    cache: PathBuf,

    /// Refresh the public archive tree index before querying.
    #[arg(long)]
    refresh: bool,

    /// Emit the complete typed comparison report as JSON.
    #[arg(long)]
    json: bool,

    /// Include reasoning and complete tool observations in human-readable output.
    #[arg(long, conflicts_with = "json")]
    full: bool,
}

impl Compare {
    pub(crate) async fn run(self) -> Result<()> {
        let mut query = PublishedQuery::new(self.task).limit(self.limit);
        if let Some(checksum) = self.checksum {
            query = query.checksum(checksum);
        }
        for agent in self.agent {
            query = query.agent(agent);
        }
        let published = PublishedResults::builder()
            .cache_directory(self.cache)
            .refresh(self.refresh)
            .build()?;
        let report = published.query(&query).await?;

        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        if self.json {
            serde_json::to_writer_pretty(&mut stdout, &report)?;
            writeln!(stdout)?;
        } else {
            write_human(&mut stdout, &report, self.full)?;
        }
        Ok(())
    }
}

fn write_human(output: &mut impl Write, report: &PublishedTask, full: bool) -> io::Result<()> {
    writeln!(
        output,
        "Published Harbor results for terminal-bench/{}",
        report.task
    )?;
    writeln!(
        output,
        "archive {} · {} results · {} passing",
        &report.archive_revision[..report.archive_revision.len().min(12)],
        report.matching_results,
        report.passing_results
    )?;
    if let Some(checksum) = &report.requested_checksum {
        writeln!(
            output,
            "exact task checksum {checksum}: {} passing",
            report.exact_passing_results
        )?;
    }
    if report.trials.is_empty() {
        writeln!(output, "\nNo matching successful published attempts.")?;
        return Ok(());
    }

    for trial in &report.trials {
        write_trial(output, report, trial, full)?;
    }
    Ok(())
}

fn write_trial(
    output: &mut impl Write,
    report: &PublishedTask,
    trial: &PublishedTrial,
    full: bool,
) -> io::Result<()> {
    let exact = report
        .requested_checksum
        .as_deref()
        .is_some_and(|checksum| checksum == trial.task_checksum);
    let revision = if report.requested_checksum.is_none() || exact {
        "exact"
    } else {
        "different revision"
    };
    let model = trial
        .agent
        .model_info
        .as_ref()
        .map_or("unknown model", |model| model.name.as_str());
    writeln!(
        output,
        "\n{} · {} · {} · reward {:.3} · {}",
        trial.submission, trial.agent.name, model, trial.reward, revision
    )?;
    writeln!(
        output,
        "trial {} · checksum {}",
        trial.trial_name, trial.task_checksum
    )?;
    let Some(trajectory) = &trial.trajectory else {
        match &trial.trajectory_error {
            Some(error) => writeln!(output, "published trajectory could not be decoded: {error}")?,
            None => writeln!(
                output,
                "trajectory unavailable in publication; pass result is cached"
            )?,
        }
        return Ok(());
    };
    writeln!(
        output,
        "{} · {} steps",
        trajectory.schema_version,
        trajectory.steps.len()
    )?;
    for step in &trajectory.steps {
        if full && let Some(reasoning) = nonempty(step.reasoning_content.as_deref()) {
            writeln!(output, "  reasoning: {reasoning}")?;
        }
        if let Some(message) = nonempty(step.message.as_deref())
            && (full || step.source == "user" || step.tool_calls.is_empty())
        {
            writeln!(output, "  {}: {}", step.source, one_line(message, full))?;
        }
        for call in &step.tool_calls {
            writeln!(
                output,
                "  tool {} {}",
                call.function_name,
                one_line(call.arguments.get(), full)
            )?;
        }
        if full && let Some(observation) = &step.observation {
            for result in &observation.results {
                writeln!(
                    output,
                    "  result {}: {}",
                    result.source_call_id, result.content
                )?;
            }
        }
    }
    Ok(())
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.trim().is_empty())
}

fn one_line(value: &str, full: bool) -> String {
    const LIMIT: usize = 240;

    if full {
        return value.to_owned();
    }
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut characters = compact.chars();
    let prefix = characters.by_ref().take(LIMIT).collect::<String>();
    if characters.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_output_flattens_and_bounds_content() {
        let input = format!("first\n{}\nlast", "x".repeat(300));
        let output = one_line(&input, false);
        assert!(!output.contains('\n'));
        assert!(output.ends_with('…'));
        assert!(output.chars().count() <= 241);
    }
}

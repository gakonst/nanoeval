use std::{env, error::Error, path::PathBuf};

use nanocodex::{Nanocodex, OpenAiAuth, Thinking};
use nanoeval::{EvalResult, Nanoeval, Task};

const K: usize = 5;
const TASKS: [&str; 3] = [
    "tasks/write-greeting",
    "tasks/uppercase-message",
    "tasks/extract-todos",
];

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let output_directory = env::args_os()
        .nth(1)
        .map_or_else(|| PathBuf::from("nanoeval-runs"), PathBuf::from);
    let [first, second, third] = TASKS.map(Task::load);
    let (first, second, third) = (first?, second?, third?);
    let agent = Nanocodex::builder(auth()?).thinking(Thinking::Low);
    let (eval, events) = Nanoeval::builder(agent)
        .output_directory(output_directory)
        .max_concurrency(TASKS.len() * K)
        .build()?;
    drop(events);

    let (first, second, third) = tokio::try_join!(
        eval.task_n(first, K),
        eval.task_n(second, K),
        eval.task_n(third, K),
    )?;

    print_results(first.into_iter().chain(second).chain(third));
    println!("Harbor jobs: {}", eval.output_directory().display());
    Ok(())
}

fn auth() -> Result<OpenAiAuth, Box<dyn Error>> {
    match env::var("OPENAI_API_KEY") {
        Ok(api_key) if !api_key.trim().is_empty() => Ok(OpenAiAuth::api_key(api_key)),
        Ok(_) | Err(env::VarError::NotPresent) => {
            let auth_file = env::var_os("NANOCODEX_AUTH_FILE")
                .map(PathBuf::from)
                .or_else(|| {
                    env::var_os("CODEX_HOME").map(|path| PathBuf::from(path).join("auth.json"))
                })
                .or_else(|| {
                    env::var_os("HOME").map(|path| PathBuf::from(path).join(".codex/auth.json"))
                })
                .ok_or("set OPENAI_API_KEY or NANOCODEX_AUTH_FILE")?;
            Ok(nanocodex::load_chatgpt_auth(auth_file)?)
        }
        Err(error) => Err(error.into()),
    }
}

fn print_results(results: impl IntoIterator<Item = EvalResult>) {
    for result in results {
        println!(
            "{}: {:?} in {} ms ({} ATIF steps)",
            result.trial_name,
            result.status,
            result.agent.metadata.duration_ms,
            result.trajectory.steps.len(),
        );
    }
}

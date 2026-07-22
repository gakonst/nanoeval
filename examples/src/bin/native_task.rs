use std::{env, error::Error, path::PathBuf};

use nanocodex::Nanocodex;
use nanoeval::{Nanoeval, Task};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let task_directory = env::args_os()
        .nth(1)
        .map_or_else(|| PathBuf::from("tasks/write-greeting"), PathBuf::from);
    let output_directory = env::args_os()
        .nth(2)
        .map_or_else(|| PathBuf::from("nanoeval-runs"), PathBuf::from);
    let auth = env::var("OPENAI_API_KEY")?;
    let task = Task::load(task_directory)?;
    let (eval, _events) = Nanoeval::builder(Nanocodex::builder(auth))
        .output_directory(output_directory)
        .build()?;

    let result = eval.task(task).await?;
    println!("{}: {:?}", result.trial_name, result.status);
    println!("{}", result.artifacts.result_json.display());
    Ok(())
}

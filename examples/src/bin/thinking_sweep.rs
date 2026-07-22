use std::{env, error::Error, path::PathBuf};

use nanocodex::{Nanocodex, OpenAiAuth, Thinking};
use nanoeval::{AgentVariant, AgentVariantSpec, EvalPlan, Nanoeval, PlannedTask, Task, TrialCount};

const K: u16 = 5;
type AnyError = Box<dyn Error + Send + Sync>;

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    let task = Task::load(
        env::args_os()
            .nth(1)
            .map_or_else(|| PathBuf::from("tasks/write-greeting"), PathBuf::from),
    )?;
    let auth = auth()?;
    let plan = EvalPlan::builder()
        .task(task, TrialCount::new(K)?)
        .variant(AgentVariant::new(
            AgentVariantSpec::new("thinking-low", Thinking::Low, "defaults")?,
            Nanocodex::builder(auth.clone()),
        ))
        .variant(AgentVariant::new(
            AgentVariantSpec::new("thinking-high", Thinking::High, "defaults")?,
            Nanocodex::builder(auth),
        ))
        .build()?;

    println!("planned {} independent attempts", plan.attempt_count());
    let mut executions = tokio::task::JoinSet::new();
    for variant in plan.variants().iter().cloned() {
        executions.spawn(run_variant(variant, plan.tasks().to_vec()));
    }
    while let Some(execution) = executions.join_next().await {
        execution??;
    }
    Ok(())
}

async fn run_variant(variant: AgentVariant, tasks: Vec<PlannedTask>) -> Result<(), AnyError> {
    let output = PathBuf::from("nanoeval-sweep-runs").join(variant.spec().id().as_str());
    let (eval, _events) = Nanoeval::builder(variant.nanocodex().clone())
        .output_directory(output)
        .max_concurrency(usize::from(K))
        .build()?;
    let attempts = tasks
        .into_iter()
        .flat_map(|task| std::iter::repeat_n(task.task().clone(), usize::from(task.trials())));
    let results = eval.tasks(attempts).await?;
    println!(
        "{}: {} completed attempts",
        variant.spec().id(),
        results.len()
    );
    Ok(())
}

fn auth() -> Result<OpenAiAuth, AnyError> {
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

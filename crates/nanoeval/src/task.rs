use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};

const TASK_CONFIG: &str = "task.toml";
const TASK_INSTRUCTION: &str = "instruction.md";
const TASK_ENVIRONMENT: &str = "environment";
const VERIFIER_SCRIPT: &str = "tests/test.sh";

/// One immutable benchmark task loaded from a Terminal-Bench task directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Task {
    root: PathBuf,
    name: Box<str>,
    description: Box<str>,
    prompt: Box<str>,
    image: OciImage,
    agent_timeout: Duration,
    verifier: Verifier,
    artifacts: Vec<PathBuf>,
    resources: Resources,
    network: NetworkPolicy,
    environment: BTreeMap<String, String>,
    requires_compose: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OciImage {
    reference: Box<str>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Verifier {
    script: PathBuf,
    timeout: Duration,
    environment: BTreeMap<String, String>,
    environment_mode: VerifierEnvironmentMode,
    collect: Vec<VerifierCollect>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VerifierEnvironmentMode {
    #[default]
    Same,
    Separate,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct VerifierCollect {
    command: Box<str>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Resources {
    pub cpus: u32,
    pub memory_mb: u64,
    pub storage_mb: u64,
    pub gpus: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkPolicy {
    Public,
    Disabled,
}

#[derive(Debug, thiserror::Error)]
pub enum TaskLoadError {
    #[error("failed to resolve task directory {path}: {source}")]
    ResolveDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to read task file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse task configuration {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("unsupported task schema version {found:?}; expected \"1.1\"")]
    UnsupportedSchema { found: String },

    #[error("task configuration {path} is invalid: {message}")]
    Invalid { path: PathBuf, message: String },

    #[error("task is missing required file {path}")]
    MissingFile { path: PathBuf },
}

impl Task {
    /// Loads the Terminal-Bench 2.1 task rooted at `directory`.
    ///
    /// # Errors
    ///
    /// Returns [`TaskLoadError`] when the directory cannot be resolved, a
    /// required task file is absent or unreadable, the TOML is malformed, or
    /// the declared Terminal-Bench 2.1 fields are invalid.
    pub fn load(directory: impl AsRef<Path>) -> Result<Self, TaskLoadError> {
        let requested = directory.as_ref();
        let root =
            fs::canonicalize(requested).map_err(|source| TaskLoadError::ResolveDirectory {
                path: requested.to_path_buf(),
                source,
            })?;
        if !root.is_dir() {
            return Err(TaskLoadError::Invalid {
                path: root,
                message: "task root is not a directory".to_owned(),
            });
        }

        let config_path = root.join(TASK_CONFIG);
        let config_text = read(&config_path)?;
        let raw: RawTask = toml::from_str(&config_text).map_err(|source| TaskLoadError::Parse {
            path: config_path.clone(),
            source,
        })?;
        if raw.schema_version != "1.1" {
            return Err(TaskLoadError::UnsupportedSchema {
                found: raw.schema_version,
            });
        }

        let instruction_path = root.join(TASK_INSTRUCTION);
        let prompt = strip_leading_canary(&read(&instruction_path)?);
        if prompt.trim().is_empty() {
            return Err(TaskLoadError::Invalid {
                path: instruction_path,
                message: "instruction is empty".to_owned(),
            });
        }

        let verifier_script = root.join(VERIFIER_SCRIPT);
        require_file(&verifier_script)?;
        let environment_directory = root.join(TASK_ENVIRONMENT);
        if !environment_directory.is_dir() {
            return Err(TaskLoadError::MissingFile {
                path: environment_directory,
            });
        }

        let name = required_string(&config_path, "task.name", raw.task.name)?;
        let image = raw
            .environment
            .docker_image
            .unwrap_or_else(|| "local-dockerfile".to_owned());

        Ok(Self {
            root,
            name: name.into_boxed_str(),
            description: raw.task.description.into_boxed_str(),
            prompt: prompt.into_boxed_str(),
            image: OciImage {
                reference: image.into_boxed_str(),
            },
            agent_timeout: duration(&config_path, "agent.timeout_sec", raw.agent.timeout_sec)?,
            verifier: Verifier {
                script: verifier_script,
                timeout: duration(
                    &config_path,
                    "verifier.timeout_sec",
                    raw.verifier.timeout_sec,
                )?,
                environment: raw.verifier.env,
                environment_mode: raw.verifier.environment_mode,
                collect: raw.verifier.collect,
            },
            artifacts: raw.artifacts,
            resources: Resources {
                cpus: positive(&config_path, "environment.cpus", raw.environment.cpus)?,
                memory_mb: positive(
                    &config_path,
                    "environment.memory_mb",
                    raw.environment.memory_mb,
                )?,
                storage_mb: positive(
                    &config_path,
                    "environment.storage_mb",
                    raw.environment.storage_mb,
                )?,
                gpus: raw.environment.gpus,
            },
            network: if raw.environment.allow_internet {
                NetworkPolicy::Public
            } else {
                NetworkPolicy::Disabled
            },
            environment: raw.environment.env,
            requires_compose: raw.metadata.custom_docker_compose
                || raw.environment.custom_docker_compose,
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    #[must_use]
    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    /// Files copied into the disposable native workspace before an attempt.
    #[must_use]
    pub fn environment_directory(&self) -> PathBuf {
        self.root.join(TASK_ENVIRONMENT)
    }

    #[must_use]
    pub const fn image(&self) -> &OciImage {
        &self.image
    }

    #[must_use]
    pub const fn agent_timeout(&self) -> Duration {
        self.agent_timeout
    }

    #[must_use]
    pub const fn verifier(&self) -> &Verifier {
        &self.verifier
    }

    #[must_use]
    pub fn artifacts(&self) -> &[PathBuf] {
        &self.artifacts
    }

    #[must_use]
    pub const fn resources(&self) -> &Resources {
        &self.resources
    }

    #[must_use]
    pub const fn network(&self) -> NetworkPolicy {
        self.network
    }

    #[must_use]
    pub fn environment(&self) -> &BTreeMap<String, String> {
        &self.environment
    }

    #[must_use]
    pub const fn requires_compose(&self) -> bool {
        self.requires_compose
    }
}

impl OciImage {
    #[must_use]
    pub fn reference(&self) -> &str {
        &self.reference
    }
}

impl NetworkPolicy {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Disabled => "no-network",
        }
    }
}

impl Verifier {
    #[must_use]
    pub fn script(&self) -> &Path {
        &self.script
    }

    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    #[must_use]
    pub fn environment(&self) -> &BTreeMap<String, String> {
        &self.environment
    }

    #[must_use]
    pub const fn environment_mode(&self) -> VerifierEnvironmentMode {
        self.environment_mode
    }

    #[must_use]
    pub fn collect(&self) -> &[VerifierCollect] {
        &self.collect
    }
}

impl VerifierCollect {
    #[must_use]
    pub fn command(&self) -> &str {
        &self.command
    }
}

impl VerifierEnvironmentMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Same => "same",
            Self::Separate => "separate",
        }
    }
}

#[derive(Deserialize)]
struct RawTask {
    schema_version: String,
    #[serde(default)]
    artifacts: Vec<PathBuf>,
    task: RawTaskInfo,
    #[serde(default)]
    metadata: RawMetadata,
    agent: RawPhase,
    verifier: RawVerifier,
    environment: RawEnvironment,
}

#[derive(Default, Deserialize)]
struct RawMetadata {
    #[serde(default)]
    custom_docker_compose: bool,
}

#[derive(Deserialize)]
struct RawTaskInfo {
    name: String,
    #[serde(default)]
    description: String,
}

#[derive(Deserialize)]
struct RawPhase {
    timeout_sec: f64,
}

#[derive(Deserialize)]
struct RawVerifier {
    timeout_sec: f64,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    environment_mode: VerifierEnvironmentMode,
    #[serde(default)]
    collect: Vec<VerifierCollect>,
}

#[derive(Deserialize)]
struct RawEnvironment {
    #[serde(default)]
    docker_image: Option<String>,
    cpus: u32,
    memory_mb: u64,
    storage_mb: u64,
    #[serde(default)]
    gpus: u32,
    #[serde(default = "enabled")]
    allow_internet: bool,
    #[serde(default)]
    custom_docker_compose: bool,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

impl<'de> Deserialize<'de> for VerifierEnvironmentMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match String::deserialize(deserializer)?.as_str() {
            "same" => Ok(Self::Same),
            "separate" => Ok(Self::Separate),
            mode => Err(serde::de::Error::unknown_variant(
                mode,
                &["same", "separate"],
            )),
        }
    }
}

impl<'de> Deserialize<'de> for VerifierCollect {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawCollect {
            command: String,
        }

        let raw = RawCollect::deserialize(deserializer)?;
        if raw.command.trim().is_empty() {
            return Err(serde::de::Error::custom(
                "verifier collect command must not be empty",
            ));
        }
        Ok(Self {
            command: raw.command.into_boxed_str(),
        })
    }
}

const fn enabled() -> bool {
    true
}

fn read(path: &Path) -> Result<String, TaskLoadError> {
    fs::read_to_string(path).map_err(|source| TaskLoadError::Read {
        path: path.to_path_buf(),
        source,
    })
}

fn require_file(path: &Path) -> Result<(), TaskLoadError> {
    if path.is_file() {
        Ok(())
    } else {
        Err(TaskLoadError::MissingFile {
            path: path.to_path_buf(),
        })
    }
}

fn required_string(path: &Path, field: &str, value: String) -> Result<String, TaskLoadError> {
    if value.trim().is_empty() {
        Err(TaskLoadError::Invalid {
            path: path.to_path_buf(),
            message: format!("{field} must not be empty"),
        })
    } else {
        Ok(value)
    }
}

fn duration(path: &Path, field: &str, seconds: f64) -> Result<Duration, TaskLoadError> {
    if seconds <= 0.0 {
        return Err(TaskLoadError::Invalid {
            path: path.to_path_buf(),
            message: format!("{field} must be greater than zero"),
        });
    }
    Duration::try_from_secs_f64(seconds).map_err(|error| TaskLoadError::Invalid {
        path: path.to_path_buf(),
        message: format!("{field} is invalid: {error}"),
    })
}

fn positive<T>(path: &Path, field: &str, value: T) -> Result<T, TaskLoadError>
where
    T: Copy + Default + PartialEq,
{
    if value == T::default() {
        Err(TaskLoadError::Invalid {
            path: path.to_path_buf(),
            message: format!("{field} must be greater than zero"),
        })
    } else {
        Ok(value)
    }
}

fn strip_leading_canary(text: &str) -> String {
    let mut lines = text.lines().peekable();
    while lines.peek().is_some_and(|line| is_canary(line)) {
        lines.next();
    }
    while lines.peek().is_some_and(|line| line.trim().is_empty()) {
        lines.next();
    }
    lines.collect::<Vec<_>>().join("\n")
}

fn is_canary(line: &str) -> bool {
    let line = line.trim();
    let comment = line.starts_with('#') || (line.starts_with("<!--") && line.ends_with("-->"));
    comment && line.to_ascii_lowercase().contains("canary")
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use tempfile::tempdir;

    use super::{NetworkPolicy, Task, VerifierEnvironmentMode};

    #[test]
    fn loads_terminal_bench_2_1_task_directory() {
        let directory = tempdir().unwrap();
        fs::create_dir(directory.path().join("tests")).unwrap();
        fs::create_dir(directory.path().join("environment")).unwrap();
        fs::write(
            directory.path().join("task.toml"),
            r#"
schema_version = "1.1"

[task]
name = "terminal-bench/example"
description = "Example task"

[metadata]
custom_docker_compose = true

[agent]
timeout_sec = 900.0

[verifier]
timeout_sec = 600.0

[verifier.env]
ANSWER = "42"

[environment]
docker_image = "example/task:20251031"
cpus = 2
memory_mb = 4096
storage_mb = 10240
gpus = 0
allow_internet = false

[environment.env]
MODE = "test"
"#,
        )
        .unwrap();
        fs::write(
            directory.path().join("instruction.md"),
            "# terminal-bench-canary secret\n\nFix the task.\n",
        )
        .unwrap();
        fs::write(directory.path().join("tests/test.sh"), "#!/bin/sh\n").unwrap();

        let task = Task::load(directory.path()).unwrap();

        assert_eq!(task.name(), "terminal-bench/example");
        assert_eq!(task.prompt(), "Fix the task.");
        assert_eq!(task.image().reference(), "example/task:20251031");
        assert_eq!(task.resources().cpus, 2);
        assert_eq!(task.network(), NetworkPolicy::Disabled);
        assert_eq!(task.environment()["MODE"], "test");
        assert_eq!(task.verifier().environment()["ANSWER"], "42");
        assert!(task.requires_compose());
    }

    #[test]
    fn rejects_missing_verifier_script() {
        let directory = tempdir().unwrap();
        fs::create_dir(directory.path().join("environment")).unwrap();
        fs::write(
            directory.path().join("task.toml"),
            r#"
schema_version = "1.1"
[task]
name = "terminal-bench/example"
[agent]
timeout_sec = 1.0
[verifier]
timeout_sec = 1.0
[environment]
docker_image = "example/task:latest"
cpus = 1
memory_mb = 1
storage_mb = 1
"#,
        )
        .unwrap();
        fs::write(directory.path().join("instruction.md"), "Do it.").unwrap();

        let error = Task::load(directory.path()).unwrap_err();
        assert!(error.to_string().contains("tests/test.sh"));
    }

    #[test]
    fn loads_frontier_bench_task_with_a_separate_verifier() {
        let directory = tempdir().unwrap();
        fs::create_dir(directory.path().join("tests")).unwrap();
        fs::create_dir(directory.path().join("environment")).unwrap();
        fs::write(
            directory.path().join("task.toml"),
            r#"
schema_version = "1.1"
artifacts = ["/app/output.txt"]

[task]
name = "terminal-bench/frontier-example"

[agent]
timeout_sec = 900.0

[verifier]
timeout_sec = 600.0
environment_mode = "separate"

[[verifier.collect]]
command = "cp /app/output.txt /tmp/output.txt"

[environment]
cpus = 2
memory_mb = 4096
storage_mb = 10240
"#,
        )
        .unwrap();
        fs::write(directory.path().join("instruction.md"), "Fix the task.").unwrap();
        fs::write(
            directory.path().join("environment/Dockerfile"),
            "FROM scratch\n",
        )
        .unwrap();
        fs::write(directory.path().join("tests/Dockerfile"), "FROM scratch\n").unwrap();
        fs::write(directory.path().join("tests/test.sh"), "#!/bin/sh\n").unwrap();

        let task = Task::load(directory.path()).unwrap();

        assert_eq!(task.image().reference(), "local-dockerfile");
        assert_eq!(
            task.verifier().environment_mode(),
            VerifierEnvironmentMode::Separate
        );
        assert_eq!(task.artifacts(), [PathBuf::from("/app/output.txt")]);
        assert_eq!(
            task.verifier().collect()[0].command(),
            "cp /app/output.txt /tmp/output.txt"
        );
    }

    #[test]
    fn loads_the_native_suite_fixtures() {
        let tasks = ["write-greeting", "uppercase-message", "extract-todos"];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tasks");

        for name in tasks {
            let task = Task::load(root.join(name)).unwrap();
            assert_eq!(task.name(), format!("nanoeval/{name}"));
            assert!(!task.prompt().is_empty());
            assert!(!task.requires_compose());
        }
    }
}

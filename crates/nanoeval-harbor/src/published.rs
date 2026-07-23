use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    path::{Path, PathBuf},
    process::ExitStatus,
    time::Duration,
};

use futures_util::{StreamExt, stream};
use reqwest::{Client, StatusCode, Url, header::RETRY_AFTER};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};
use tokio::{fs, process::Command};
use uuid::Uuid;

const DEFAULT_REPOSITORY: &str =
    "https://huggingface.co/datasets/harborframework/terminal-bench-2-leaderboard";
const DEFAULT_DOWNLOAD_BASE: &str =
    "https://huggingface.co/datasets/harborframework/terminal-bench-2-leaderboard/resolve/";
const MAX_ARTIFACT_BYTES: usize = 64 * 1024 * 1024;
const DOWNLOAD_CONCURRENCY: usize = 4;
const DOWNLOAD_ATTEMPTS: u32 = 8;

#[derive(Debug, thiserror::Error)]
pub enum PublishedError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Http(#[from] reqwest::Error),

    #[error(transparent)]
    Url(#[from] url::ParseError),

    #[error("published-results cache path has no parent: {0}")]
    MissingCacheParent(PathBuf),

    #[error("git {operation} failed with {status}: {stderr}")]
    Git {
        operation: &'static str,
        status: ExitStatus,
        stderr: String,
    },

    #[error("git returned non-UTF-8 output while {0}")]
    GitUtf8(&'static str),

    #[error("published artifact is larger than {limit} bytes: {url}")]
    ArtifactTooLarge { limit: usize, url: Url },

    #[error("published artifact does not exist: {0}")]
    MissingArtifact(Url),

    #[error("invalid published result path: {0}")]
    InvalidResultPath(String),
}

#[derive(Clone, Debug)]
pub struct PublishedResultsBuilder {
    cache_directory: PathBuf,
    repository: String,
    download_base: String,
    refresh: bool,
}

impl PublishedResultsBuilder {
    #[must_use]
    pub fn cache_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.cache_directory = directory.into();
        self
    }

    /// Refreshes the archive's small Git tree index before querying.
    #[must_use]
    pub const fn refresh(mut self, refresh: bool) -> Self {
        self.refresh = refresh;
        self
    }

    /// Overrides the metadata repository and artifact base, primarily for mirrors.
    #[must_use]
    pub fn source(mut self, repository: impl Into<String>, download_base: &str) -> Self {
        self.repository = repository.into();
        download_base.clone_into(&mut self.download_base);
        self
    }

    /// Builds the reader after validating its artifact URL.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured artifact base is not a valid URL.
    pub fn build(self) -> Result<PublishedResults, PublishedError> {
        Ok(PublishedResults {
            cache_directory: self.cache_directory,
            repository: self.repository,
            download_base: Url::parse(&self.download_base)?,
            refresh: self.refresh,
            client: Client::new(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct PublishedResults {
    cache_directory: PathBuf,
    repository: String,
    download_base: Url,
    refresh: bool,
    client: Client,
}

impl PublishedResults {
    /// Builds a cached reader for Harbor's public Terminal-Bench archive.
    ///
    #[must_use]
    pub fn builder() -> PublishedResultsBuilder {
        PublishedResultsBuilder {
            cache_directory: PathBuf::from(".cache/nanoeval/published"),
            repository: DEFAULT_REPOSITORY.to_owned(),
            download_base: DEFAULT_DOWNLOAD_BASE.to_owned(),
            refresh: false,
        }
    }

    /// Finds successful published attempts for one task and downloads their
    /// typed result and ATIF artifacts into the content-addressed local cache.
    ///
    /// # Errors
    ///
    /// Returns an error when the archive index cannot be prepared, an artifact
    /// cannot be downloaded, or a published JSON document is malformed.
    pub async fn query(&self, query: &PublishedQuery) -> Result<PublishedTask, PublishedError> {
        let index = self.prepare_index().await?;
        let revision = self
            .git_output(&index, "read archive revision", &["rev-parse", "HEAD"])
            .await?
            .trim()
            .to_owned();
        let candidates = self.load_task_paths(&index, &revision, &query.task).await?;
        let matching_results = candidates.len();

        let records = self
            .load_task_results(&revision, &query.task, candidates)
            .await?;

        let mut passing = Vec::new();
        for record in records {
            let PublishedRecord { candidate, result } = record;
            let reward = result.reward();
            if reward <= 0.0 || result.exception_info.is_some() {
                continue;
            }
            if !query.agent_matches(&candidate.submission, &result.agent_info) {
                continue;
            }
            passing.push((candidate, result, reward));
        }
        passing.sort_by(|left, right| {
            let left_exact = query.matches_checksum(&left.1.task_checksum);
            let right_exact = query.matches_checksum(&right.1.task_checksum);
            right_exact
                .cmp(&left_exact)
                .then_with(|| {
                    right
                        .0
                        .trajectory
                        .is_some()
                        .cmp(&left.0.trajectory.is_some())
                })
                .then_with(|| left.0.submission.cmp(&right.0.submission))
        });

        let passing_results = passing.len();
        let exact_passing_results = passing
            .iter()
            .filter(|(_, result, _)| query.matches_checksum(&result.task_checksum))
            .count();
        let mut submissions = BTreeSet::new();
        let selected = passing
            .into_iter()
            .filter(|(candidate, _, _)| submissions.insert(candidate.submission.clone()))
            .take(query.limit)
            .collect::<Vec<_>>();
        let published = self;
        let archive_revision = &revision;
        let trials = stream::iter(selected.into_iter().map(
            move |(candidate, result, reward)| async move {
                let (trajectory, trajectory_error) = match candidate.trajectory.as_deref() {
                    Some(trajectory_path) => {
                        match published.download(archive_revision, trajectory_path).await {
                            Ok(bytes) => match decode_published_trajectory(&bytes) {
                                Ok(trajectory) => (Some(trajectory), None),
                                Err(error) => (None, Some(error.to_string())),
                            },
                            Err(error) => (None, Some(error.to_string())),
                        }
                    }
                    None => (None, None),
                };
                Ok::<_, PublishedError>(PublishedTrial {
                    submission: candidate.submission,
                    run: candidate.run,
                    trial_name: result.trial_name,
                    task_name: result.task_name,
                    task_checksum: result.task_checksum,
                    reward,
                    agent: result.agent_info,
                    result_path: candidate.result,
                    trajectory_path: candidate.trajectory,
                    trajectory,
                    trajectory_error,
                })
            },
        ))
        .buffered(DOWNLOAD_CONCURRENCY)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;

        Ok(PublishedTask {
            task: query.task.clone(),
            requested_checksum: query.checksum.clone(),
            archive_revision: revision,
            matching_results,
            passing_results,
            exact_passing_results,
            trials,
        })
    }

    async fn prepare_index(&self) -> Result<PathBuf, PublishedError> {
        let index = self.cache_directory.join("terminal-bench-2-leaderboard");
        if !index.join(".git").is_dir() {
            let parent = index
                .parent()
                .ok_or_else(|| PublishedError::MissingCacheParent(index.clone()))?;
            fs::create_dir_all(parent).await?;
            let output = Command::new("git")
                .args([
                    "clone",
                    "--filter=blob:none",
                    "--no-checkout",
                    "--depth",
                    "1",
                    &self.repository,
                ])
                .arg(&index)
                .output()
                .await?;
            ensure_git("clone archive index", output.status, &output.stderr)?;
        } else if self.refresh {
            let output = Command::new("git")
                .current_dir(&index)
                .args(["fetch", "--depth", "1", "origin", "main"])
                .output()
                .await?;
            ensure_git("refresh archive index", output.status, &output.stderr)?;
            let output = Command::new("git")
                .current_dir(&index)
                .args(["update-ref", "HEAD", "FETCH_HEAD"])
                .output()
                .await?;
            ensure_git(
                "select refreshed archive index",
                output.status,
                &output.stderr,
            )?;
        }
        Ok(index)
    }

    async fn load_task_results(
        &self,
        revision: &str,
        task: &str,
        candidates: Vec<PublishedPath>,
    ) -> Result<Vec<PublishedRecord>, PublishedError> {
        let task_key = format!("{:x}", Sha256::digest(task.as_bytes()));
        let manifest = self
            .cache_directory
            .join("task-results")
            .join(revision)
            .join(format!("{task_key}.json"));
        if manifest.is_file() {
            return Ok(serde_json::from_slice(&fs::read(manifest).await?)?);
        }

        let downloads = stream::iter(candidates.into_iter().map(|candidate| async {
            let bytes = self.download(revision, &candidate.result).await?;
            let result = serde_json::from_slice::<PublishedResult>(&bytes)?;
            Ok::<_, PublishedError>(PublishedRecord { candidate, result })
        }))
        .buffer_unordered(DOWNLOAD_CONCURRENCY)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
        if let Some(parent) = manifest.parent() {
            fs::create_dir_all(parent).await?;
        }
        let temporary = manifest.with_extension(format!("{}.tmp", Uuid::new_v4()));
        fs::write(&temporary, serde_json::to_vec(&downloads)?).await?;
        fs::rename(temporary, manifest).await?;
        Ok(downloads)
    }

    async fn load_task_paths(
        &self,
        index: &Path,
        revision: &str,
        task: &str,
    ) -> Result<Vec<PublishedPath>, PublishedError> {
        let task_key = format!("{:x}", Sha256::digest(task.as_bytes()));
        let manifest = self
            .cache_directory
            .join("task-paths")
            .join(revision)
            .join(format!("{task_key}.json"));
        if manifest.is_file() {
            return Ok(serde_json::from_slice(&fs::read(manifest).await?)?);
        }

        let tree = self
            .git_output(
                index,
                "list archive tree",
                &["ls-tree", "-r", "--name-only", "HEAD"],
            )
            .await?;
        let entries = tree.lines().collect::<HashSet<_>>();
        let mut candidates = entries
            .iter()
            .filter(|entry| is_task_result(entry, task))
            .map(|entry| PublishedPath::parse(entry, &entries))
            .collect::<Result<Vec<_>, _>>()?;
        candidates.sort_by(|left, right| left.result.cmp(&right.result));
        if let Some(parent) = manifest.parent() {
            fs::create_dir_all(parent).await?;
        }
        let temporary = manifest.with_extension(format!("{}.tmp", Uuid::new_v4()));
        fs::write(&temporary, serde_json::to_vec(&candidates)?).await?;
        fs::rename(temporary, manifest).await?;
        Ok(candidates)
    }

    async fn git_output(
        &self,
        index: &Path,
        operation: &'static str,
        arguments: &[&str],
    ) -> Result<String, PublishedError> {
        let output = Command::new("git")
            .current_dir(index)
            .args(arguments)
            .output()
            .await?;
        ensure_git(operation, output.status, &output.stderr)?;
        String::from_utf8(output.stdout).map_err(|_| PublishedError::GitUtf8(operation))
    }

    async fn download(&self, revision: &str, artifact: &str) -> Result<Vec<u8>, PublishedError> {
        let cached = self
            .cache_directory
            .join("artifacts")
            .join(revision)
            .join(artifact);
        if cached.is_file() {
            return Ok(fs::read(cached).await?);
        }

        let mut url = self.download_base.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|()| PublishedError::MissingCacheParent(cached.clone()))?;
            segments.pop_if_empty();
            segments.push(revision);
            for segment in artifact.split('/') {
                segments.push(segment);
            }
        }
        let mut attempt = 0;
        let response = loop {
            let response = self.client.get(url.clone()).send().await?;
            if response.status() != StatusCode::TOO_MANY_REQUESTS
                && !response.status().is_server_error()
            {
                break response;
            }
            attempt += 1;
            if attempt >= DOWNLOAD_ATTEMPTS {
                break response;
            }
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .map(Duration::from_secs);
            let exponential = Duration::from_millis(250 * 2_u64.pow(attempt - 1));
            tokio::time::sleep(retry_after.unwrap_or(exponential)).await;
        };
        if response.status() == StatusCode::NOT_FOUND {
            return Err(PublishedError::MissingArtifact(url));
        }
        let response = response.error_for_status()?;
        if response.content_length().is_some_and(|size| {
            usize::try_from(size).map_or(true, |size| size > MAX_ARTIFACT_BYTES)
        }) {
            return Err(PublishedError::ArtifactTooLarge {
                limit: MAX_ARTIFACT_BYTES,
                url,
            });
        }
        let bytes = response.bytes().await?;
        if bytes.len() > MAX_ARTIFACT_BYTES {
            return Err(PublishedError::ArtifactTooLarge {
                limit: MAX_ARTIFACT_BYTES,
                url,
            });
        }
        if let Some(parent) = cached.parent() {
            fs::create_dir_all(parent).await?;
        }
        let temporary = cached.with_extension(format!("{}.tmp", Uuid::new_v4()));
        fs::write(&temporary, &bytes).await?;
        fs::rename(temporary, &cached).await?;
        Ok(bytes.to_vec())
    }
}

#[derive(Clone, Debug)]
pub struct PublishedQuery {
    task: String,
    checksum: Option<String>,
    limit: usize,
    agents: Vec<String>,
}

impl PublishedQuery {
    #[must_use]
    pub fn new(task: impl Into<String>) -> Self {
        let task = task.into();
        Self {
            task: task
                .strip_prefix("terminal-bench/")
                .unwrap_or(&task)
                .to_owned(),
            checksum: None,
            limit: 10,
            agents: Vec::new(),
        }
    }

    #[must_use]
    pub fn checksum(mut self, checksum: impl Into<String>) -> Self {
        self.checksum = Some(checksum.into());
        self
    }

    #[must_use]
    pub const fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    #[must_use]
    pub fn agent(mut self, agent: impl Into<String>) -> Self {
        self.agents.push(agent.into());
        self
    }

    fn matches_checksum(&self, checksum: &str) -> bool {
        self.checksum
            .as_deref()
            .is_some_and(|expected| checksum == expected)
    }

    fn agent_matches(&self, submission: &str, agent: &PublishedAgentInfo) -> bool {
        self.agents.is_empty()
            || self.agents.iter().any(|needle| {
                let needle = needle.to_lowercase();
                submission.to_lowercase().contains(&needle)
                    || agent.name.to_lowercase().contains(&needle)
                    || agent
                        .model_info
                        .as_ref()
                        .is_some_and(|model| model.name.to_lowercase().contains(&needle))
            })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct PublishedTask {
    pub task: String,
    pub requested_checksum: Option<String>,
    pub archive_revision: String,
    pub matching_results: usize,
    pub passing_results: usize,
    pub exact_passing_results: usize,
    pub trials: Vec<PublishedTrial>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PublishedTrial {
    pub submission: String,
    pub run: String,
    pub trial_name: String,
    pub task_name: String,
    pub task_checksum: String,
    pub reward: f64,
    pub agent: PublishedAgentInfo,
    pub result_path: String,
    pub trajectory_path: Option<String>,
    pub trajectory: Option<PublishedTrajectory>,
    pub trajectory_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PublishedAgentInfo {
    pub name: String,
    pub version: Option<String>,
    pub model_info: Option<PublishedModelInfo>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PublishedModelInfo {
    pub name: String,
    pub provider: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PublishedResult {
    task_name: String,
    task_checksum: String,
    trial_name: String,
    agent_info: PublishedAgentInfo,
    verifier_result: Option<PublishedVerifierResult>,
    exception_info: Option<Box<RawValue>>,
}

impl PublishedResult {
    fn reward(&self) -> f64 {
        self.verifier_result
            .as_ref()
            .and_then(|result| result.rewards.get("reward"))
            .copied()
            .unwrap_or(0.0)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PublishedVerifierResult {
    rewards: BTreeMap<String, f64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PublishedTrajectory {
    pub schema_version: String,
    pub session_id: Option<String>,
    pub agent: PublishedAgent,
    pub steps: Vec<PublishedStep>,
}

#[derive(Debug, Deserialize)]
struct WrappedPublishedTrajectory {
    atif_trajectory: PublishedTrajectory,
}

fn decode_published_trajectory(bytes: &[u8]) -> Result<PublishedTrajectory, serde_json::Error> {
    match serde_json::from_slice(bytes) {
        Ok(trajectory) => Ok(trajectory),
        Err(_) => serde_json::from_slice::<WrappedPublishedTrajectory>(bytes)
            .map(|wrapped| wrapped.atif_trajectory),
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum PublishedAgent {
    Details(PublishedAgentDetails),
    Name(String),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PublishedAgentDetails {
    pub name: String,
    pub version: Option<String>,
    pub model_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PublishedStep {
    pub step_id: PublishedStepId,
    pub timestamp: Option<String>,
    pub source: String,
    pub model_name: Option<String>,
    #[serde(alias = "content")]
    pub message: Option<String>,
    pub reasoning_content: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_vec")]
    pub tool_calls: Vec<PublishedToolCall>,
    pub observation: Option<PublishedObservation>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum PublishedStepId {
    Number(u64),
    Text(String),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PublishedToolCall {
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(alias = "tool_name")]
    pub function_name: String,
    #[serde(alias = "parameters")]
    pub arguments: Box<RawValue>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PublishedObservation {
    #[serde(default)]
    pub results: Vec<PublishedObservationResult>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PublishedObservationResult {
    pub source_call_id: String,
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PublishedPath {
    submission: String,
    run: String,
    result: String,
    trajectory: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct PublishedRecord {
    candidate: PublishedPath,
    result: PublishedResult,
}

impl PublishedPath {
    fn parse(result: &str, entries: &HashSet<&str>) -> Result<Self, PublishedError> {
        let components = result.split('/').collect::<Vec<_>>();
        if components.len() < 7 {
            return Err(PublishedError::InvalidResultPath(result.to_owned()));
        }
        let submission = components
            .get(3)
            .ok_or_else(|| PublishedError::InvalidResultPath(result.to_owned()))?
            .to_string();
        let run = components
            .get(4)
            .ok_or_else(|| PublishedError::InvalidResultPath(result.to_owned()))?
            .to_string();
        let trajectory = format!(
            "{}/agent/trajectory.json",
            result
                .strip_suffix("/result.json")
                .ok_or_else(|| PublishedError::InvalidResultPath(result.to_owned()))?
        );
        Ok(Self {
            submission,
            run,
            result: result.to_owned(),
            trajectory: entries.contains(trajectory.as_str()).then_some(trajectory),
        })
    }
}

fn is_task_result(entry: &str, task: &str) -> bool {
    entry.ends_with("/result.json")
        && entry
            .rsplit('/')
            .nth(1)
            .is_some_and(|trial| trial.starts_with(&format!("{task}__")))
}

fn ensure_git(
    operation: &'static str,
    status: ExitStatus,
    stderr: &[u8],
) -> Result<(), PublishedError> {
    if status.success() {
        Ok(())
    } else {
        Err(PublishedError::Git {
            operation,
            status,
            stderr: String::from_utf8_lossy(stderr).trim().to_owned(),
        })
    }
}

fn deserialize_optional_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<Vec<T>>::deserialize(deserializer).map(Option::unwrap_or_default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_result_matching_is_segment_aware() {
        assert!(is_task_result(
            "submissions/terminal-bench/2.0/a/run/configure-git-webserver__abc/result.json",
            "configure-git-webserver"
        ));
        assert!(!is_task_result(
            "submissions/terminal-bench/2.0/a/run/not-configure-git-webserver__abc/result.json",
            "configure-git-webserver"
        ));
    }

    #[test]
    fn published_path_finds_optional_trajectory() {
        let result = "submissions/terminal-bench/2.0/agent/run/task__abc/result.json".to_owned();
        let trajectory =
            "submissions/terminal-bench/2.0/agent/run/task__abc/agent/trajectory.json".to_owned();
        let entries = HashSet::from([result.as_str(), trajectory.as_str()]);
        let parsed = PublishedPath::parse(&result, &entries).unwrap();
        assert_eq!(parsed.submission, "agent");
        assert_eq!(parsed.run, "run");
        assert_eq!(parsed.trajectory.as_deref(), Some(trajectory.as_str()));
    }

    #[test]
    fn query_normalizes_package_prefix() {
        let query = PublishedQuery::new("terminal-bench/task");
        assert_eq!(query.task, "task");
    }
}

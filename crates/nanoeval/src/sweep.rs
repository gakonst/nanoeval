use std::{fmt, num::NonZeroU16, path::PathBuf};

use nanocodex::{NanocodexBuilder, StandardResponses};
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

use crate::Task;

/// Stable caller-defined identity for one agent configuration in a sweep.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct AgentId(Box<str>);

/// A finite task-by-agent-by-trial evaluation sweep.
#[derive(Clone, Debug)]
pub struct Sweep {
    tasks: Vec<Task>,
    agents: Vec<SweepAgent>,
    trials: NonZeroU16,
    attempt_count: usize,
}

/// Builder for an advanced multi-agent evaluation sweep.
pub struct SweepBuilder {
    tasks: Vec<Task>,
    agents: Vec<SweepAgent>,
    trials: u16,
}

#[derive(Clone)]
struct SweepAgent {
    id: AgentId,
    nanocodex: NanocodexBuilder<StandardResponses>,
}

#[derive(Clone, Copy)]
pub(crate) struct SweepAttempt<'a> {
    task: &'a Task,
    agent: &'a SweepAgent,
    trial: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub(crate) struct RunManifest {
    tasks: Vec<RunTask>,
    agents: Vec<AgentId>,
    trials: NonZeroU16,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
struct RunTask {
    root: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AgentIdError {
    #[error("agent identifier must not be empty")]
    Empty,

    #[error("agent identifier `{value}` must begin with an ASCII letter or digit")]
    InvalidStart { value: String },

    #[error("agent identifier `{value}` contains invalid character `{character}`")]
    InvalidCharacter { value: String, character: char },
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SweepError {
    #[error("an evaluation sweep requires at least one task")]
    NoTasks,

    #[error("an evaluation sweep requires at least one agent")]
    NoAgents,

    #[error("sweep trial count must be greater than zero")]
    ZeroTrials,

    #[error("task `{0}` appears more than once in the evaluation sweep")]
    DuplicateTask(String),

    #[error("agent `{0}` appears more than once in the evaluation sweep")]
    DuplicateAgent(AgentId),

    #[error("evaluation sweep contains too many attempts")]
    TooManyAttempts,
}

impl AgentId {
    /// Creates a filesystem-safe stable agent identity.
    ///
    /// # Errors
    ///
    /// Returns an error when `value` is empty or contains characters other
    /// than ASCII letters, digits, `.`, `_`, or `-`.
    pub fn new(value: impl Into<String>) -> Result<Self, AgentIdError> {
        let value = value.into();
        if value.is_empty() {
            return Err(AgentIdError::Empty);
        }
        if !value.starts_with(|character: char| character.is_ascii_alphanumeric()) {
            return Err(AgentIdError::InvalidStart { value });
        }
        if let Some(character) = value.chars().find(|character| {
            !character.is_ascii_alphanumeric() && !matches!(character, '.' | '_' | '-')
        }) {
            return Err(AgentIdError::InvalidCharacter { value, character });
        }
        Ok(Self(value.into_boxed_str()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AgentId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

impl Sweep {
    #[must_use]
    pub fn builder() -> SweepBuilder {
        SweepBuilder {
            tasks: Vec::new(),
            agents: Vec::new(),
            trials: 1,
        }
    }

    #[must_use]
    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    #[must_use]
    pub fn agents(&self) -> impl ExactSizeIterator<Item = &AgentId> {
        self.agents.iter().map(|agent| &agent.id)
    }

    #[must_use]
    pub const fn trials(&self) -> u16 {
        self.trials.get()
    }

    #[must_use]
    pub const fn attempt_count(&self) -> usize {
        self.attempt_count
    }

    pub(crate) fn attempts(&self) -> impl Iterator<Item = SweepAttempt<'_>> {
        let agents = &self.agents;
        let trials = self.trials.get();
        self.tasks.iter().flat_map(move |task| {
            agents.iter().flat_map(move |agent| {
                (1..=trials).map(move |trial| SweepAttempt { task, agent, trial })
            })
        })
    }

    pub(crate) fn manifest(&self) -> RunManifest {
        RunManifest {
            tasks: self
                .tasks
                .iter()
                .map(|task| RunTask {
                    root: task.root().to_path_buf(),
                })
                .collect(),
            agents: self.agents.iter().map(|agent| agent.id.clone()).collect(),
            trials: self.trials,
        }
    }
}

impl fmt::Debug for SweepAgent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SweepAgent")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl SweepBuilder {
    #[must_use]
    pub fn tasks(mut self, tasks: Vec<Task>) -> Self {
        self.tasks = tasks;
        self
    }

    #[must_use]
    pub fn task(mut self, task: Task) -> Self {
        self.tasks.push(task);
        self
    }

    #[must_use]
    pub const fn trials(mut self, trials: u16) -> Self {
        self.trials = trials;
        self
    }

    /// Adds one independently configured Nanocodex recipe.
    ///
    /// # Errors
    ///
    /// Returns an error when `id` is not a filesystem-safe stable identity.
    pub fn agent(
        mut self,
        id: impl Into<String>,
        nanocodex: NanocodexBuilder<StandardResponses>,
    ) -> Result<Self, AgentIdError> {
        self.agents.push(SweepAgent {
            id: AgentId::new(id)?,
            nanocodex: nanocodex.shared_prompt_cache(),
        });
        Ok(self)
    }

    /// Validates uniqueness and fixes deterministic task-agent-trial order.
    ///
    /// # Errors
    ///
    /// Returns an error for empty or duplicate inputs, zero trials, or an
    /// attempt count that does not fit in [`usize`].
    pub fn build(self) -> Result<Sweep, SweepError> {
        if self.tasks.is_empty() {
            return Err(SweepError::NoTasks);
        }
        if self.agents.is_empty() {
            return Err(SweepError::NoAgents);
        }
        let trials = NonZeroU16::new(self.trials).ok_or(SweepError::ZeroTrials)?;
        for (index, task) in self.tasks.iter().enumerate() {
            if self.tasks[..index]
                .iter()
                .any(|other| other.root() == task.root())
            {
                return Err(SweepError::DuplicateTask(task.root().display().to_string()));
            }
        }
        for (index, agent) in self.agents.iter().enumerate() {
            if self.agents[..index]
                .iter()
                .any(|other| other.id == agent.id)
            {
                return Err(SweepError::DuplicateAgent(agent.id.clone()));
            }
        }
        let attempt_count = self
            .tasks
            .len()
            .checked_mul(self.agents.len())
            .and_then(|count| count.checked_mul(usize::from(trials.get())))
            .ok_or(SweepError::TooManyAttempts)?;
        Ok(Sweep {
            tasks: self.tasks,
            agents: self.agents,
            trials,
            attempt_count,
        })
    }
}

impl SweepAttempt<'_> {
    pub(crate) const fn task(&self) -> &Task {
        self.task
    }

    pub(crate) const fn agent_id(&self) -> &AgentId {
        &self.agent.id
    }

    pub(crate) const fn nanocodex(&self) -> &NanocodexBuilder<StandardResponses> {
        &self.agent.nanocodex
    }

    pub(crate) const fn trial(&self) -> u16 {
        self.trial
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use nanocodex::Nanocodex;

    use super::*;

    #[test]
    fn expands_task_agent_trial_product_in_stable_order() {
        let sweep = Sweep::builder()
            .tasks(vec![
                load_task("write-greeting"),
                load_task("uppercase-message"),
            ])
            .trials(2)
            .agent("low", Nanocodex::builder("test-key"))
            .unwrap()
            .agent("high", Nanocodex::builder("test-key"))
            .unwrap()
            .build()
            .unwrap();

        let expanded = sweep
            .attempts()
            .map(|attempt| {
                (
                    attempt.task().name().to_owned(),
                    attempt.agent_id().as_str().to_owned(),
                    attempt.trial(),
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(sweep.attempt_count(), 8);
        assert_eq!(expanded[0].1, "low");
        assert_eq!(expanded[0].2, 1);
        assert_eq!(expanded[1].2, 2);
        assert_eq!(expanded[2].1, "high");
        assert_eq!(expanded[4].0, "nanoeval/uppercase-message");
    }

    #[test]
    fn rejects_unsafe_and_duplicate_agent_ids() {
        assert!(matches!(
            AgentId::new("mcp/on"),
            Err(AgentIdError::InvalidCharacter { character: '/', .. })
        ));
        let error = Sweep::builder()
            .task(load_task("write-greeting"))
            .agent("same", Nanocodex::builder("test-key"))
            .unwrap()
            .agent("same", Nanocodex::builder("test-key"))
            .unwrap()
            .build()
            .unwrap_err();
        assert_eq!(
            error,
            SweepError::DuplicateAgent(AgentId::new("same").unwrap())
        );
    }

    fn load_task(name: &str) -> Task {
        Task::load(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../tasks")
                .join(name),
        )
        .unwrap()
    }
}

use std::{fmt, num::NonZeroU16};

use nanocodex::{NanocodexBuilder, StandardResponses, Thinking};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

use crate::Task;

/// Stable caller-defined identity for one agent configuration in a sweep.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct AgentVariantId(Box<str>);

/// Stable caller-defined identity for the tools installed in an agent variant.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ToolProfileId(Box<str>);

/// The number of independent attempts requested for one task and variant.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct TrialCount(NonZeroU16);

/// A one-based trial position within one task and agent variant.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(transparent)]
pub struct TrialOrdinal(NonZeroU16);

/// Serializable identity and comparison dimensions for an agent variant.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct AgentVariantSpec {
    id: AgentVariantId,
    #[serde(with = "thinking_serde")]
    thinking: Thinking,
    tools: ToolProfileId,
}

/// One named Nanocodex recipe in an evaluation sweep.
#[derive(Clone)]
pub struct AgentVariant {
    spec: AgentVariantSpec,
    nanocodex: NanocodexBuilder<StandardResponses>,
}

/// One task and its requested trial count in a finite evaluation plan.
#[derive(Clone, Debug)]
pub struct PlannedTask {
    task: Task,
    trials: TrialCount,
}

/// A finite task-by-agent evaluation plan.
#[derive(Clone, Debug)]
pub struct EvalPlan {
    tasks: Vec<PlannedTask>,
    variants: Vec<AgentVariant>,
    attempt_count: usize,
}

/// Builder for a finite deterministic evaluation plan.
#[derive(Default)]
pub struct EvalPlanBuilder {
    tasks: Vec<PlannedTask>,
    variants: Vec<AgentVariant>,
}

/// One borrowed task-by-agent-by-ordinal entry from an [`EvalPlan`].
#[derive(Clone, Copy)]
pub struct PlannedAttempt<'a> {
    task: &'a Task,
    variant: &'a AgentVariant,
    ordinal: TrialOrdinal,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PlanIdError {
    #[error("plan identifier must not be empty")]
    Empty,

    #[error("plan identifier `{value}` must begin with an ASCII letter or digit")]
    InvalidStart { value: String },

    #[error("plan identifier `{value}` contains invalid character `{character}`")]
    InvalidCharacter { value: String, character: char },
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum TrialCountError {
    #[error("trial count must be greater than zero")]
    Zero,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum EvalPlanError {
    #[error("an evaluation plan requires at least one task")]
    NoTasks,

    #[error("an evaluation plan requires at least one agent variant")]
    NoVariants,

    #[error("task `{0}` appears more than once in the evaluation plan")]
    DuplicateTask(String),

    #[error("agent variant `{0}` appears more than once in the evaluation plan")]
    DuplicateVariant(AgentVariantId),

    #[error("evaluation plan contains too many attempts")]
    TooManyAttempts,
}

impl AgentVariantId {
    /// Creates a filesystem-safe stable variant identity.
    ///
    /// # Errors
    ///
    /// Returns an error when `value` is empty or contains characters other
    /// than ASCII letters, digits, `.`, `_`, or `-`, or when it does not begin
    /// with a letter or digit.
    pub fn new(value: impl Into<String>) -> Result<Self, PlanIdError> {
        validate_id(value).map(|value| Self(value.into_boxed_str()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentVariantId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AgentVariantId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

impl ToolProfileId {
    /// Creates a stable identity for a caller-defined tool configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when `value` is empty or contains characters other
    /// than ASCII letters, digits, `.`, `_`, or `-`, or when it does not begin
    /// with a letter or digit.
    pub fn new(value: impl Into<String>) -> Result<Self, PlanIdError> {
        validate_id(value).map(|value| Self(value.into_boxed_str()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ToolProfileId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

impl TrialCount {
    /// Creates a non-zero trial count.
    ///
    /// # Errors
    ///
    /// Returns an error when `count` is zero.
    pub const fn new(count: u16) -> Result<Self, TrialCountError> {
        match NonZeroU16::new(count) {
            Some(count) => Ok(Self(count)),
            None => Err(TrialCountError::Zero),
        }
    }

    #[must_use]
    pub const fn get(self) -> u16 {
        self.0.get()
    }
}

impl From<TrialCount> for usize {
    fn from(count: TrialCount) -> Self {
        usize::from(count.get())
    }
}

impl TrialOrdinal {
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0.get()
    }
}

impl AgentVariantSpec {
    /// Creates the durable descriptor for one caller-configured agent recipe.
    ///
    /// # Errors
    ///
    /// Returns an error when either stable identifier is invalid.
    pub fn new(
        id: impl Into<String>,
        thinking: Thinking,
        tools: impl Into<String>,
    ) -> Result<Self, PlanIdError> {
        Ok(Self {
            id: AgentVariantId::new(id)?,
            thinking,
            tools: ToolProfileId::new(tools)?,
        })
    }

    #[must_use]
    pub const fn id(&self) -> &AgentVariantId {
        &self.id
    }

    #[must_use]
    pub const fn thinking(&self) -> Thinking {
        self.thinking
    }

    #[must_use]
    pub const fn tools(&self) -> &ToolProfileId {
        &self.tools
    }
}

impl AgentVariant {
    #[must_use]
    pub fn new(spec: AgentVariantSpec, nanocodex: NanocodexBuilder<StandardResponses>) -> Self {
        let nanocodex = nanocodex.thinking(spec.thinking);
        Self { spec, nanocodex }
    }

    #[must_use]
    pub const fn spec(&self) -> &AgentVariantSpec {
        &self.spec
    }

    /// Returns the cloneable recipe used to create a fresh agent session for
    /// every planned attempt.
    #[must_use]
    pub const fn nanocodex(&self) -> &NanocodexBuilder<StandardResponses> {
        &self.nanocodex
    }
}

impl fmt::Debug for AgentVariant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentVariant")
            .field("spec", &self.spec)
            .finish_non_exhaustive()
    }
}

impl PlannedTask {
    #[must_use]
    pub const fn new(task: Task, trials: TrialCount) -> Self {
        Self { task, trials }
    }

    #[must_use]
    pub const fn task(&self) -> &Task {
        &self.task
    }

    #[must_use]
    pub const fn trials(&self) -> TrialCount {
        self.trials
    }
}

impl EvalPlan {
    #[must_use]
    pub fn builder() -> EvalPlanBuilder {
        EvalPlanBuilder::default()
    }

    #[must_use]
    pub fn tasks(&self) -> &[PlannedTask] {
        &self.tasks
    }

    #[must_use]
    pub fn variants(&self) -> &[AgentVariant] {
        &self.variants
    }

    #[must_use]
    pub const fn attempt_count(&self) -> usize {
        self.attempt_count
    }

    /// Expands tasks, then variants, then one-based trial ordinals in stable
    /// insertion order.
    pub fn attempts(&self) -> impl Iterator<Item = PlannedAttempt<'_>> {
        self.tasks.iter().flat_map(|task| {
            self.variants.iter().flat_map(move |variant| {
                (0..task.trials.get()).map(move |offset| PlannedAttempt {
                    task: &task.task,
                    variant,
                    ordinal: TrialOrdinal(NonZeroU16::MIN.saturating_add(offset)),
                })
            })
        })
    }
}

impl EvalPlanBuilder {
    #[must_use]
    pub fn task(mut self, task: Task, trials: TrialCount) -> Self {
        self.tasks.push(PlannedTask::new(task, trials));
        self
    }

    #[must_use]
    pub fn variant(mut self, variant: AgentVariant) -> Self {
        self.variants.push(variant);
        self
    }

    /// Validates uniqueness and fixes the plan's deterministic expansion.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty plan, duplicate task roots or variant
    /// identities, or an attempt count that does not fit in [`usize`].
    pub fn build(self) -> Result<EvalPlan, EvalPlanError> {
        if self.tasks.is_empty() {
            return Err(EvalPlanError::NoTasks);
        }
        if self.variants.is_empty() {
            return Err(EvalPlanError::NoVariants);
        }
        for (index, task) in self.tasks.iter().enumerate() {
            if self.tasks[..index]
                .iter()
                .any(|other| other.task.root() == task.task.root())
            {
                return Err(EvalPlanError::DuplicateTask(
                    task.task.root().display().to_string(),
                ));
            }
        }
        for (index, variant) in self.variants.iter().enumerate() {
            if self.variants[..index]
                .iter()
                .any(|other| other.spec.id == variant.spec.id)
            {
                return Err(EvalPlanError::DuplicateVariant(variant.spec.id.clone()));
            }
        }
        let trials = self.tasks.iter().try_fold(0_usize, |total, task| {
            total.checked_add(usize::from(task.trials))
        });
        let attempt_count = trials
            .and_then(|trials| trials.checked_mul(self.variants.len()))
            .ok_or(EvalPlanError::TooManyAttempts)?;
        Ok(EvalPlan {
            tasks: self.tasks,
            variants: self.variants,
            attempt_count,
        })
    }
}

impl PlannedAttempt<'_> {
    #[must_use]
    pub const fn task(&self) -> &Task {
        self.task
    }

    #[must_use]
    pub const fn variant(&self) -> &AgentVariant {
        self.variant
    }

    #[must_use]
    pub const fn ordinal(&self) -> TrialOrdinal {
        self.ordinal
    }
}

fn validate_id(value: impl Into<String>) -> Result<String, PlanIdError> {
    let value = value.into();
    if value.is_empty() {
        return Err(PlanIdError::Empty);
    }
    if !value.starts_with(|character: char| character.is_ascii_alphanumeric()) {
        return Err(PlanIdError::InvalidStart { value });
    }
    if let Some(character) = value.chars().find(|character| {
        !character.is_ascii_alphanumeric() && !matches!(character, '.' | '_' | '-')
    }) {
        return Err(PlanIdError::InvalidCharacter { value, character });
    }
    Ok(value)
}

mod thinking_serde {
    use super::*;

    // Serde's `serialize_with` contract requires a borrowed field.
    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub fn serialize<S>(thinking: &Thinking, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(thinking.as_str())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Thinking, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use nanocodex::Nanocodex;

    use super::*;

    #[test]
    fn expands_task_variant_trial_product_in_stable_order() {
        let first = load_task("write-greeting");
        let second = load_task("uppercase-message");
        let low = variant("low", Thinking::Low, "defaults");
        let high = variant("high-mcp", Thinking::High, "docs-mcp");
        let plan = EvalPlan::builder()
            .task(first, TrialCount::new(2).unwrap())
            .task(second, TrialCount::new(1).unwrap())
            .variant(low)
            .variant(high)
            .build()
            .unwrap();

        let expanded = plan
            .attempts()
            .map(|attempt| {
                (
                    attempt.task().name().to_owned(),
                    attempt.variant().spec().id().as_str().to_owned(),
                    attempt.ordinal().get(),
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(plan.attempt_count(), 6);
        assert_eq!(
            expanded,
            [
                ("nanoeval/write-greeting".to_owned(), "low".to_owned(), 1),
                ("nanoeval/write-greeting".to_owned(), "low".to_owned(), 2),
                (
                    "nanoeval/write-greeting".to_owned(),
                    "high-mcp".to_owned(),
                    1
                ),
                (
                    "nanoeval/write-greeting".to_owned(),
                    "high-mcp".to_owned(),
                    2
                ),
                ("nanoeval/uppercase-message".to_owned(), "low".to_owned(), 1),
                (
                    "nanoeval/uppercase-message".to_owned(),
                    "high-mcp".to_owned(),
                    1
                ),
            ]
        );
    }

    #[test]
    fn rejects_unsafe_ids_and_duplicate_variants() {
        assert!(matches!(
            AgentVariantId::new("mcp/on"),
            Err(PlanIdError::InvalidCharacter { character: '/', .. })
        ));
        assert!(matches!(
            AgentVariantId::new(".."),
            Err(PlanIdError::InvalidStart { .. })
        ));

        let task = load_task("write-greeting");
        let error = EvalPlan::builder()
            .task(task, TrialCount::new(1).unwrap())
            .variant(variant("same", Thinking::Low, "defaults"))
            .variant(variant("same", Thinking::High, "defaults"))
            .build()
            .unwrap_err();
        assert_eq!(
            error,
            EvalPlanError::DuplicateVariant(AgentVariantId::new("same").unwrap())
        );
    }

    #[test]
    fn serializes_variant_spec_without_dynamic_json_fields() {
        let spec = AgentVariantSpec::new("low-mcp", Thinking::Low, "docs-mcp").unwrap();
        let encoded = serde_json::to_string(&spec).unwrap();
        let decoded: AgentVariantSpec = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded, spec);
        assert_eq!(
            encoded,
            r#"{"id":"low-mcp","thinking":"low","tools":"docs-mcp"}"#
        );
        assert!(serde_json::from_str::<AgentVariantId>(r#""mcp/on""#).is_err());
    }

    fn variant(id: &str, thinking: Thinking, tools: &str) -> AgentVariant {
        AgentVariant::new(
            AgentVariantSpec::new(id, thinking, tools).unwrap(),
            Nanocodex::builder("test-key"),
        )
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

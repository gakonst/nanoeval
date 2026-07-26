use std::{collections::BTreeSet, fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

const MAX_CAPABILITY_NAME_BYTES: usize = 64;

/// A caller-requestable capability name such as `network.public` or
/// `github.read`.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CapabilityName(String);

impl CapabilityName {
    /// Parses and validates a capability name.
    ///
    /// # Errors
    ///
    /// Returns an error unless the name contains lowercase dot-separated
    /// identifiers and fits the API's bounded representation.
    pub fn new(value: impl Into<String>) -> Result<Self, CapabilityNameError> {
        let value = value.into();
        validate_capability_name(&value)?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CapabilityName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("CapabilityName")
            .field(&self.0)
            .finish()
    }
}

impl fmt::Display for CapabilityName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for CapabilityName {
    type Err = CapabilityNameError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for CapabilityName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for CapabilityName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

/// Flat, closed capability set fixed for the lifetime of one managed agent.
///
/// The server maps names to concrete tools, egress routes, and secret
/// providers. Clients never provide proxy URLs or credentials.
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct AgentCapabilities(BTreeSet<CapabilityName>);

impl AgentCapabilities {
    #[must_use]
    pub fn new(capabilities: impl IntoIterator<Item = CapabilityName>) -> Self {
        Self(capabilities.into_iter().collect())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.0.iter().any(|capability| capability.as_str() == name)
    }

    #[must_use]
    pub const fn names(&self) -> &BTreeSet<CapabilityName> {
        &self.0
    }

    #[must_use]
    pub fn is_subset(&self, allowed: &Self) -> bool {
        self.0.is_subset(&allowed.0)
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum CapabilityNameError {
    #[error("capability names must contain 1 to {MAX_CAPABILITY_NAME_BYTES} bytes")]
    Length,
    #[error(
        "capability names must be lowercase dot-separated identifiers using letters, digits, `_`, or `-`"
    )]
    Syntax,
}

fn validate_capability_name(value: &str) -> Result<(), CapabilityNameError> {
    if value.is_empty() || value.len() > MAX_CAPABILITY_NAME_BYTES {
        return Err(CapabilityNameError::Length);
    }
    if value.split('.').all(valid_segment) {
        Ok(())
    } else {
        Err(CapabilityNameError::Syntax)
    }
}

fn valid_segment(segment: &str) -> bool {
    let mut bytes = segment.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_namespaced_capabilities() {
        let capability = CapabilityName::new("github.pull-requests_read").unwrap();
        assert_eq!(capability.as_str(), "github.pull-requests_read");
    }

    #[test]
    fn rejects_ambiguous_or_unbounded_names() {
        for invalid in [
            "",
            ".github.read",
            "github..read",
            "GitHub.read",
            "github/read",
            "9github.read",
        ] {
            assert!(CapabilityName::new(invalid).is_err(), "{invalid}");
        }
        assert!(CapabilityName::new("a".repeat(65)).is_err());
    }

    #[test]
    fn request_shape_is_a_deduplicated_flat_list() {
        let capabilities = serde_json::from_str::<AgentCapabilities>(
            r#"["github.read","tools.web_search","github.read"]"#,
        )
        .unwrap();
        assert_eq!(capabilities.names().len(), 2);
    }
}

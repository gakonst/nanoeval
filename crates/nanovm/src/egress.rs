use std::{any::Any, collections::BTreeMap, fmt, path::PathBuf, sync::Arc};

use thiserror::Error;

use crate::Network;

/// VM-facing outbound-access configuration retained for one guest lifetime.
///
/// An application-specific provider can resolve MPP, secret, or capability
/// policy into this type without exposing that policy to `nanovm`. Values are
/// deliberately omitted from `Debug`: proxy URLs may contain short-lived
/// credentials.
#[derive(Clone)]
pub struct EgressLease {
    network: Network,
    guest_environment: BTreeMap<String, String>,
    guest_mounts: BTreeMap<String, EgressMount>,
    guards: Vec<Arc<dyn Any + Send + Sync>>,
}

/// One provider-owned host directory mounted read-only into the guest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EgressMount {
    pub tag: String,
    pub host_path: PathBuf,
    pub guest_path: PathBuf,
}

impl EgressLease {
    #[must_use]
    pub fn new(network: Network) -> Self {
        Self {
            network,
            guest_environment: BTreeMap::new(),
            guest_mounts: BTreeMap::new(),
            guards: Vec::new(),
        }
    }

    #[must_use]
    pub fn internet() -> Self {
        Self::new(Network::Internet)
    }

    #[must_use]
    pub fn disabled() -> Self {
        Self::new(Network::Disabled)
    }

    /// Adds one guest environment value.
    ///
    /// # Errors
    ///
    /// Returns an error when the name is empty or was already assigned a
    /// different value by another egress component.
    pub fn insert_environment(
        &mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<(), EgressError> {
        let name = name.into();
        if !valid_environment_name(&name) {
            return Err(EgressError::InvalidEnvironmentName(name));
        }
        let value = value.into();
        if self
            .guest_environment
            .get(&name)
            .is_some_and(|current| current != &value)
        {
            return Err(EgressError::EnvironmentConflict(name));
        }
        self.guest_environment.insert(name, value);
        Ok(())
    }

    /// Adds one read-only provider mount.
    ///
    /// # Errors
    ///
    /// Returns an error when the tag or guest path collides with a different
    /// mount.
    pub fn insert_mount(&mut self, mount: EgressMount) -> Result<(), EgressError> {
        if mount.tag.is_empty() {
            return Err(EgressError::EmptyMountTag);
        }
        if self
            .guest_mounts
            .values()
            .any(|current| current.guest_path == mount.guest_path && current != &mount)
        {
            return Err(EgressError::GuestMountConflict(mount.guest_path));
        }
        if self
            .guest_mounts
            .get(&mount.tag)
            .is_some_and(|current| current != &mount)
        {
            return Err(EgressError::MountTagConflict(mount.tag));
        }
        self.guest_mounts.insert(mount.tag.clone(), mount);
        Ok(())
    }

    /// Retains provider state, such as a revocable proxy lease, until the guest
    /// is dropped.
    pub fn retain<T>(&mut self, guard: Arc<T>)
    where
        T: Any + Send + Sync,
    {
        self.guards.push(guard);
    }

    /// Combines independently provisioned egress fragments.
    ///
    /// Identical network, environment, and mount configuration is idempotent.
    /// Conflicting configuration fails closed.
    ///
    /// # Errors
    ///
    /// Returns an error when the fragments select different network modes or
    /// assign incompatible environment or mount values.
    pub fn merge(&mut self, other: Self) -> Result<(), EgressError> {
        if self.network != other.network {
            return Err(EgressError::NetworkConflict);
        }
        let mut merged = self.clone();
        for (name, value) in other.guest_environment {
            merged.insert_environment(name, value)?;
        }
        for mount in other.guest_mounts.into_values() {
            merged.insert_mount(mount)?;
        }
        merged.guards.extend(other.guards);
        *self = merged;
        Ok(())
    }

    #[must_use]
    pub const fn network(&self) -> &Network {
        &self.network
    }

    #[must_use]
    pub fn guest_environment(&self) -> &BTreeMap<String, String> {
        &self.guest_environment
    }

    pub fn guest_mounts(&self) -> impl Iterator<Item = &EgressMount> {
        self.guest_mounts.values()
    }
}

impl fmt::Debug for EgressLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EgressLease")
            .field("network", &self.network)
            .field(
                "guest_environment_keys",
                &self.guest_environment.keys().collect::<Vec<_>>(),
            )
            .field(
                "guest_mounts",
                &self.guest_mounts.values().collect::<Vec<_>>(),
            )
            .field("guards", &self.guards.len())
            .finish()
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum EgressError {
    #[error("egress fragments require conflicting VM network modes")]
    NetworkConflict,
    #[error("guest environment name `{0}` is not a shell identifier")]
    InvalidEnvironmentName(String),
    #[error("guest environment `{0}` has conflicting egress values")]
    EnvironmentConflict(String),
    #[error("egress mount tag must not be empty")]
    EmptyMountTag,
    #[error("egress mount tag `{0}` has conflicting host paths")]
    MountTagConflict(String),
    #[error("guest egress mount path `{0}` has conflicting providers")]
    GuestMountConflict(PathBuf),
}

fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independently_provisioned_egress_fragments_compose() {
        let guard = Arc::new(());
        let mut secrets = EgressLease::internet();
        secrets
            .insert_environment("OPENAI_BASE_URL", "https://gateway/v1/openai")
            .unwrap();
        secrets
            .insert_mount(EgressMount {
                tag: "secret-ca".to_owned(),
                host_path: PathBuf::from("/host/ca"),
                guest_path: PathBuf::from("/run/egress/ca"),
            })
            .unwrap();
        secrets.retain(Arc::clone(&guard));

        let mut mpp = EgressLease::internet();
        mpp.insert_environment("MPP_ENDPOINT", "https://gateway/v1/mpp")
            .unwrap();
        secrets.merge(mpp).unwrap();

        assert_eq!(secrets.guest_environment().len(), 2);
        assert_eq!(secrets.guest_mounts().count(), 1);
        assert_eq!(Arc::strong_count(&guard), 2);
        assert!(!format!("{secrets:?}").contains("https://gateway"));
    }

    #[test]
    fn conflicting_provider_values_fail_closed() {
        let mut secrets = EgressLease::internet();
        secrets
            .insert_environment("HTTPS_PROXY", "http://secret-gateway")
            .unwrap();
        let mut mpp = EgressLease::internet();
        mpp.insert_environment("HTTPS_PROXY", "http://mpp-gateway")
            .unwrap();

        assert_eq!(
            secrets.merge(mpp),
            Err(EgressError::EnvironmentConflict("HTTPS_PROXY".to_owned()))
        );
    }
}

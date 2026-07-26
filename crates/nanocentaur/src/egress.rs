use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::PathBuf,
    sync::Arc,
};

use async_trait::async_trait;
use nanovm::Network;
use thiserror::Error;
use url::Url;

use crate::CapabilityName;

/// Server-derived identity supplied to an egress provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EgressContext {
    pub agent_id: String,
    pub principal: String,
}

/// Network and process environment retained for one managed agent.
///
/// Values are deliberately omitted from `Debug`: proxy URLs can contain
/// short-lived credentials.
#[derive(Clone)]
pub struct EgressLease {
    network: Network,
    guest_environment: Vec<(String, String)>,
    guest_mounts: Vec<EgressMount>,
    guards: Vec<Arc<dyn Any + Send + Sync>>,
}

/// One provider-owned host directory mounted into the guest runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EgressMount {
    pub tag: String,
    pub host_path: PathBuf,
    pub guest_path: PathBuf,
}

impl EgressLease {
    #[must_use]
    pub fn new(
        network: Network,
        guest_environment: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        Self {
            network,
            guest_environment: guest_environment.into_iter().collect(),
            guest_mounts: Vec::new(),
            guards: Vec::new(),
        }
    }

    /// Retains a provider-owned lifecycle guard until the agent is dropped.
    #[must_use]
    pub fn with_guard(mut self, guard: Arc<dyn Any + Send + Sync>) -> Self {
        self.guards.push(guard);
        self
    }

    #[must_use]
    pub(crate) fn with_environment(
        mut self,
        environment: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        self.guest_environment.extend(environment);
        self
    }

    #[must_use]
    pub(crate) fn with_network(mut self, network: Network) -> Self {
        self.network = network;
        self
    }

    #[must_use]
    pub(crate) fn with_mount(mut self, mount: EgressMount) -> Self {
        self.guest_mounts.push(mount);
        self
    }

    #[must_use]
    pub const fn network(&self) -> &Network {
        &self.network
    }

    #[must_use]
    pub fn guest_environment(&self) -> &[(String, String)] {
        &self.guest_environment
    }

    #[must_use]
    pub fn guest_mounts(&self) -> &[EgressMount] {
        &self.guest_mounts
    }
}

impl fmt::Debug for EgressLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EgressLease")
            .field("network", &self.network)
            .field(
                "guest_environment_keys",
                &self
                    .guest_environment
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .field("guest_mounts", &self.guest_mounts)
            .field("guards", &self.guards.len())
            .finish()
    }
}

/// Swappable policy/provisioning boundary for VM outbound access.
#[async_trait]
pub trait EgressProvider: Send + Sync {
    /// Resolves authenticated, caller-requested capability names into one
    /// concrete VM route.
    async fn acquire(
        &self,
        context: &EgressContext,
        requested: &BTreeSet<CapabilityName>,
    ) -> Result<EgressLease, EgressError>;
}

/// One server-configured proxy route. Callers refer only to capabilities, never
/// to this URL or its credentials.
#[derive(Clone)]
pub struct ProxyProfile {
    name: String,
    proxy_url: String,
    no_proxy: String,
    ca_certificate: Option<PathBuf>,
}

impl ProxyProfile {
    /// Creates a proxy profile after validating its URL.
    ///
    /// # Errors
    ///
    /// Returns an error unless the URL is an absolute HTTP(S) proxy URL.
    pub fn new(name: impl Into<String>, proxy_url: impl Into<String>) -> Result<Self, EgressError> {
        let name = name.into();
        let proxy_url = proxy_url.into();
        let parsed = Url::parse(&proxy_url).map_err(|_| EgressError::InvalidProxyUrl)?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(EgressError::InvalidProxyUrl);
        }
        Ok(Self {
            name,
            proxy_url,
            no_proxy: String::new(),
            ca_certificate: None,
        })
    }

    #[must_use]
    pub fn no_proxy(mut self, value: impl Into<String>) -> Self {
        self.no_proxy = value.into();
        self
    }

    /// Sets the certificate path as it appears inside the guest rootfs.
    #[must_use]
    pub fn ca_certificate(mut self, path: impl Into<PathBuf>) -> Self {
        self.ca_certificate = Some(path.into());
        self
    }

    fn environment(&self) -> Vec<(String, String)> {
        let mut environment = vec![
            ("http_proxy".to_owned(), self.proxy_url.clone()),
            ("https_proxy".to_owned(), self.proxy_url.clone()),
            ("HTTP_PROXY".to_owned(), self.proxy_url.clone()),
            ("HTTPS_PROXY".to_owned(), self.proxy_url.clone()),
            ("no_proxy".to_owned(), self.no_proxy.clone()),
            ("NO_PROXY".to_owned(), self.no_proxy.clone()),
        ];
        if let Some(certificate) = &self.ca_certificate {
            let certificate = certificate.to_string_lossy().into_owned();
            environment.extend([
                ("SSL_CERT_FILE".to_owned(), certificate.clone()),
                ("REQUESTS_CA_BUNDLE".to_owned(), certificate.clone()),
                ("CURL_CA_BUNDLE".to_owned(), certificate),
            ]);
        }
        environment
    }
}

impl fmt::Debug for ProxyProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProxyProfile")
            .field("name", &self.name)
            .field("proxy_url", &"<redacted>")
            .field("no_proxy", &self.no_proxy)
            .field("ca_certificate", &self.ca_certificate)
            .finish()
    }
}

#[derive(Clone, Debug)]
enum Route {
    Direct,
    Proxy(Arc<ProxyProfile>),
}

/// Static capability-to-route policy suitable for direct access or a
/// separately managed proxy such as iron-proxy.
#[derive(Clone, Debug, Default)]
pub struct CapabilityEgress {
    routes: BTreeMap<CapabilityName, Route>,
}

impl CapabilityEgress {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn direct(mut self, capability: CapabilityName) -> Self {
        self.routes.insert(capability, Route::Direct);
        self
    }

    #[must_use]
    pub fn proxy(mut self, capability: CapabilityName, profile: Arc<ProxyProfile>) -> Self {
        self.routes.insert(capability, Route::Proxy(profile));
        self
    }

    #[must_use]
    pub fn configured_capabilities(&self) -> BTreeSet<CapabilityName> {
        self.routes.keys().cloned().collect()
    }
}

#[async_trait]
impl EgressProvider for CapabilityEgress {
    async fn acquire(
        &self,
        _context: &EgressContext,
        requested: &BTreeSet<CapabilityName>,
    ) -> Result<EgressLease, EgressError> {
        if requested.is_empty() {
            return Ok(EgressLease::new(Network::Disabled, []));
        }

        let mut proxy: Option<&Arc<ProxyProfile>> = None;
        for capability in requested {
            match self
                .routes
                .get(capability)
                .ok_or_else(|| EgressError::CapabilityDenied(capability.clone()))?
            {
                Route::Direct => {}
                Route::Proxy(candidate) => {
                    if proxy.is_some_and(|selected| selected.name != candidate.name) {
                        return Err(EgressError::ConflictingProxyProfiles);
                    }
                    proxy = Some(candidate);
                }
            }
        }

        Ok(proxy.map_or_else(
            || EgressLease::new(Network::Internet, []),
            |profile| EgressLease::new(Network::Internet, profile.environment()),
        ))
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum EgressError {
    #[error("egress capability `{0}` is not granted by this server")]
    CapabilityDenied(CapabilityName),
    #[error("requested capabilities require conflicting proxy profiles")]
    ConflictingProxyProfiles,
    #[error("proxy profile must use an absolute HTTP(S) URL")]
    InvalidProxyUrl,
    #[error("egress provider failed: {0}")]
    Provider(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capability(name: &str) -> CapabilityName {
        CapabilityName::new(name).unwrap()
    }

    #[tokio::test]
    async fn no_capabilities_fail_closed_without_a_network_device() {
        let lease = CapabilityEgress::new()
            .acquire(
                &EgressContext {
                    agent_id: "test".to_owned(),
                    principal: "test".to_owned(),
                },
                &BTreeSet::new(),
            )
            .await
            .unwrap();
        assert_eq!(lease.network(), &Network::Disabled);
        assert!(lease.guest_environment().is_empty());
    }

    #[tokio::test]
    async fn proxy_capability_injects_only_server_owned_configuration() {
        let profile = Arc::new(
            ProxyProfile::new("iron", "http://ephemeral:secret@proxy.internal:8080")
                .unwrap()
                .ca_certificate("/etc/iron-proxy/ca.crt"),
        );
        let policy = CapabilityEgress::new().proxy(capability("github.read"), Arc::clone(&profile));
        let lease = policy
            .acquire(
                &EgressContext {
                    agent_id: "test".to_owned(),
                    principal: "test".to_owned(),
                },
                &BTreeSet::from([capability("github.read")]),
            )
            .await
            .unwrap();

        assert_eq!(lease.network(), &Network::Internet);
        assert!(
            lease
                .guest_environment()
                .iter()
                .any(|(name, value)| name == "HTTPS_PROXY" && value.contains("proxy.internal"))
        );
        assert!(!format!("{lease:?}").contains("secret"));
    }

    #[tokio::test]
    async fn unknown_capabilities_are_denied() {
        let error = CapabilityEgress::new()
            .acquire(
                &EgressContext {
                    agent_id: "test".to_owned(),
                    principal: "test".to_owned(),
                },
                &BTreeSet::from([capability("github.write")]),
            )
            .await
            .unwrap_err();
        assert_eq!(
            error,
            EgressError::CapabilityDenied(capability("github.write"))
        );
    }
}

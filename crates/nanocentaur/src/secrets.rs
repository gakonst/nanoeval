use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

mod onepassword_connect;
mod onepassword_sdk;

pub use onepassword_connect::{OnePasswordConnectConfigError, OnePasswordConnectSecretManager};
pub use onepassword_sdk::{
    ONEPASSWORD_CORE_SHA256, ONEPASSWORD_CORE_URL, ONEPASSWORD_CORE_VERSION,
    OnePasswordSdkConfigError, OnePasswordSdkSecretManager,
};

const MAX_SECRET_NAME_BYTES: usize = 256;
const MAX_ENV_NAME_BYTES: usize = 128;
const MAX_HEADER_NAME_BYTES: usize = 128;
const MAX_PATH_PREFIX_BYTES: usize = 2_048;

/// Opaque server-side reference. It is configuration, never secret material.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRef {
    pub provider: String,
    pub key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSecret {
    pub id: Option<String>,
    pub name: String,
    pub source: SecretRef,
    pub upstream: String,
    #[serde(default)]
    pub rules: Vec<SecretRequestRule>,
    pub delivery: SecretDelivery,
    pub guest: SecretGuestConfig,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PatchSecret {
    pub name: Option<String>,
    pub source: Option<SecretRef>,
    pub upstream: Option<String>,
    pub rules: Option<Vec<SecretRequestRule>>,
    pub delivery: Option<SecretDelivery>,
    pub guest: Option<SecretGuestConfig>,
    pub enabled: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SecretView {
    pub id: String,
    pub name: String,
    pub source: SecretRef,
    pub upstream: String,
    pub rules: Vec<SecretRequestRule>,
    pub delivery: SecretDelivery,
    pub guest: SecretGuestConfig,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRequestRule {
    #[serde(default)]
    pub methods: BTreeSet<SecretHttpMethod>,
    #[serde(default)]
    pub path_prefixes: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum SecretHttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
}

impl SecretHttpMethod {
    #[must_use]
    pub fn matches(self, method: &axum::http::Method) -> bool {
        matches!(
            (self, method),
            (Self::Get, &axum::http::Method::GET)
                | (Self::Post, &axum::http::Method::POST)
                | (Self::Put, &axum::http::Method::PUT)
                | (Self::Patch, &axum::http::Method::PATCH)
                | (Self::Delete, &axum::http::Method::DELETE)
                | (Self::Head, &axum::http::Method::HEAD)
                | (Self::Options, &axum::http::Method::OPTIONS)
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SecretDelivery {
    InjectHeader {
        header: String,
        #[serde(default)]
        prefix: String,
    },
    ReplaceHeader {
        header: String,
        placeholder: String,
    },
}

impl SecretDelivery {
    #[must_use]
    pub fn header(&self) -> &str {
        match self {
            Self::InjectHeader { header, .. } | Self::ReplaceHeader { header, .. } => header,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecretGuestConfig {
    pub base_url_env: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder_env: Option<String>,
}

pub(crate) fn validate_secret(secret: &CreateSecret) -> Result<(), SecretConfigError> {
    if let Some(id) = &secret.id {
        validate_id(id)?;
    }
    validate_name(&secret.name)?;
    validate_source(&secret.source)?;
    validate_upstream(&secret.upstream)?;
    validate_rules(&secret.rules)?;
    validate_delivery(&secret.delivery)?;
    validate_env_name(&secret.guest.base_url_env)?;
    if let Some(placeholder) = &secret.guest.placeholder_env {
        validate_env_name(placeholder)?;
    }
    if let SecretDelivery::ReplaceHeader { placeholder, .. } = &secret.delivery {
        validate_env_name(placeholder)?;
        if secret.guest.placeholder_env.as_deref() != Some(placeholder.as_str()) {
            return Err(SecretConfigError::PlaceholderMismatch);
        }
    }
    Ok(())
}

fn validate_id(value: &str) -> Result<(), SecretConfigError> {
    if value.is_empty()
        || value.len() > MAX_SECRET_NAME_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        Err(SecretConfigError::InvalidId)
    } else {
        Ok(())
    }
}

fn validate_name(value: &str) -> Result<(), SecretConfigError> {
    if value.trim().is_empty() || value.len() > MAX_SECRET_NAME_BYTES {
        Err(SecretConfigError::InvalidName)
    } else {
        Ok(())
    }
}

fn validate_source(source: &SecretRef) -> Result<(), SecretConfigError> {
    if source.provider.trim().is_empty()
        || source.key.trim().is_empty()
        || source.provider.len() > MAX_SECRET_NAME_BYTES
        || source.key.len() > 4 * 1_024
    {
        Err(SecretConfigError::InvalidSource)
    } else {
        Ok(())
    }
}

fn validate_upstream(value: &str) -> Result<(), SecretConfigError> {
    let upstream = Url::parse(value).map_err(|_| SecretConfigError::InvalidUpstream)?;
    if !matches!(upstream.scheme(), "http" | "https")
        || upstream.host_str().is_none()
        || !upstream.username().is_empty()
        || upstream.password().is_some()
        || upstream.query().is_some()
        || upstream.fragment().is_some()
    {
        return Err(SecretConfigError::InvalidUpstream);
    }
    Ok(())
}

fn validate_rules(rules: &[SecretRequestRule]) -> Result<(), SecretConfigError> {
    for rule in rules {
        for prefix in &rule.path_prefixes {
            if !prefix.starts_with('/')
                || prefix.len() > MAX_PATH_PREFIX_BYTES
                || prefix.split('/').any(|segment| segment == "..")
            {
                return Err(SecretConfigError::InvalidPathPrefix);
            }
        }
    }
    Ok(())
}

fn validate_delivery(delivery: &SecretDelivery) -> Result<(), SecretConfigError> {
    let header = delivery.header();
    if header.is_empty()
        || header.len() > MAX_HEADER_NAME_BYTES
        || axum::http::HeaderName::from_bytes(header.as_bytes()).is_err()
        || header.eq_ignore_ascii_case("host")
        || header.eq_ignore_ascii_case("content-length")
        || header.eq_ignore_ascii_case("transfer-encoding")
        || header.eq_ignore_ascii_case("connection")
        || header.eq_ignore_ascii_case("proxy-authorization")
    {
        return Err(SecretConfigError::InvalidHeader);
    }
    Ok(())
}

fn validate_env_name(value: &str) -> Result<(), SecretConfigError> {
    let mut bytes = value.bytes();
    let valid_start = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_uppercase() || byte == b'_');
    if value.len() > MAX_ENV_NAME_BYTES
        || !valid_start
        || !bytes.all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    {
        Err(SecretConfigError::InvalidEnvironmentName)
    } else {
        Ok(())
    }
}

/// Pluggable boundary for environment, Vault, cloud KMS, or Iron credentials.
#[async_trait]
pub trait SecretManager: Send + Sync {
    async fn resolve(&self, reference: &SecretRef) -> Result<String, SecretError>;
}

/// Resolves keys beneath a configured host directory, suitable for Docker
/// secrets, tmpfs mounts, or a secret-store CSI projection.
pub struct FileSecretManager {
    root: PathBuf,
}

impl FileSecretManager {
    /// Creates a file-backed provider rooted at one canonical directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the root cannot be canonicalized.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, SecretError> {
        let root = std::fs::canonicalize(root).map_err(SecretError::Io)?;
        if !root.is_dir() {
            return Err(SecretError::InvalidRoot);
        }
        Ok(Self { root })
    }
}

#[async_trait]
impl SecretManager for FileSecretManager {
    async fn resolve(&self, reference: &SecretRef) -> Result<String, SecretError> {
        let relative = Path::new(&reference.key);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(SecretError::InvalidKey);
        }
        let path = self.root.join(relative);
        let canonical = tokio::fs::canonicalize(&path).await.map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                SecretError::NotFound {
                    provider: reference.provider.clone(),
                    key: reference.key.clone(),
                }
            } else {
                SecretError::Io(error)
            }
        })?;
        if !canonical.starts_with(&self.root) {
            return Err(SecretError::InvalidKey);
        }
        tokio::fs::read_to_string(canonical)
            .await
            .map(|value| value.trim_end_matches(['\r', '\n']).to_owned())
            .map_err(SecretError::Io)
    }
}

/// Resolves explicitly prefixed environment variables on the host.
pub struct EnvironmentSecretManager {
    prefix: String,
}

impl EnvironmentSecretManager {
    #[must_use]
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
        }
    }
}

#[async_trait]
impl SecretManager for EnvironmentSecretManager {
    async fn resolve(&self, reference: &SecretRef) -> Result<String, SecretError> {
        let name = format!("{}{}", self.prefix, reference.key);
        env::var(&name).map_err(|_| SecretError::NotFound {
            provider: reference.provider.clone(),
            key: reference.key.clone(),
        })
    }
}

/// Routes references by provider name without coupling the agent runtime to a
/// particular secret backend.
#[derive(Default)]
pub struct CompositeSecretManager {
    providers: BTreeMap<String, Arc<dyn SecretManager>>,
}

impl CompositeSecretManager {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn provider(mut self, name: impl Into<String>, provider: Arc<dyn SecretManager>) -> Self {
        self.providers.insert(name.into(), provider);
        self
    }
}

#[async_trait]
impl SecretManager for CompositeSecretManager {
    async fn resolve(&self, reference: &SecretRef) -> Result<String, SecretError> {
        self.providers
            .get(&reference.provider)
            .ok_or_else(|| SecretError::UnknownProvider(reference.provider.clone()))?
            .resolve(reference)
            .await
    }
}

#[derive(Debug, Error)]
pub enum SecretError {
    #[error("unknown secret provider `{0}`")]
    UnknownProvider(String),
    #[error("secret `{provider}:{key}` was not found")]
    NotFound { provider: String, key: String },
    #[error("secret provider failed: {0}")]
    Provider(String),
    #[error("secret reference `{provider}:{key}` is invalid")]
    InvalidReference { provider: String, key: String },
    #[error("secret key is not a safe relative path")]
    InvalidKey,
    #[error("secret file provider root must be a directory")]
    InvalidRoot,
    #[error("secret file provider failed")]
    Io(#[source] std::io::Error),
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SecretConfigError {
    #[error("secret id must use 1 to 256 ASCII letters, digits, dots, dashes, or underscores")]
    InvalidId,
    #[error("secret name must be non-empty and bounded")]
    InvalidName,
    #[error("secret source provider and key must be non-empty and bounded")]
    InvalidSource,
    #[error("secret upstream must be an absolute HTTP(S) origin without credentials or query")]
    InvalidUpstream,
    #[error("secret rule path prefixes must be absolute, bounded, and contain no parent traversal")]
    InvalidPathPrefix,
    #[error("secret delivery header is invalid or unsafe")]
    InvalidHeader,
    #[error("secret guest environment names must use uppercase shell identifier syntax")]
    InvalidEnvironmentName,
    #[error("replace delivery placeholder must equal guest.placeholder_env")]
    PlaceholderMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> CreateSecret {
        CreateSecret {
            id: Some("openai".to_owned()),
            name: "OpenAI".to_owned(),
            source: SecretRef {
                provider: "environment".to_owned(),
                key: "OPENAI".to_owned(),
            },
            upstream: "https://api.openai.com".to_owned(),
            rules: Vec::new(),
            delivery: SecretDelivery::InjectHeader {
                header: "authorization".to_owned(),
                prefix: "Bearer ".to_owned(),
            },
            guest: SecretGuestConfig {
                base_url_env: "OPENAI_BASE_URL".to_owned(),
                placeholder_env: None,
            },
        }
    }

    #[test]
    fn rejects_origins_with_credentials_and_unsafe_ids() {
        let mut secret = request();
        secret.upstream = "https://token@example.com".to_owned();
        assert_eq!(
            validate_secret(&secret),
            Err(SecretConfigError::InvalidUpstream)
        );
        secret.upstream = "https://example.com".to_owned();
        secret.id = Some("../escape".to_owned());
        assert_eq!(validate_secret(&secret), Err(SecretConfigError::InvalidId));
    }

    #[tokio::test]
    async fn file_provider_cannot_escape_its_root() {
        let directory = tempfile::tempdir().unwrap();
        let manager = FileSecretManager::new(directory.path()).unwrap();
        let error = manager
            .resolve(&SecretRef {
                provider: "file".to_owned(),
                key: "../outside".to_owned(),
            })
            .await
            .unwrap_err();
        assert!(matches!(error, SecretError::InvalidKey));
    }
}

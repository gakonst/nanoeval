use std::time::Duration;

use async_trait::async_trait;
use futures_util::TryStreamExt;
use reqwest::{
    Client, StatusCode,
    header::{ACCEPT, AUTHORIZATION, HeaderValue},
    redirect::Policy,
};
use serde::Deserialize;
use thiserror::Error;
use url::Url;

use super::{SecretError, SecretManager, SecretRef};

const MAX_CONNECT_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_REFERENCE_SEGMENT_BYTES: usize = 256;

/// Resolves `op://vault/item/[section/]field` references against a 1Password
/// Connect server. The Connect access token stays in this host-side client.
pub struct OnePasswordConnectSecretManager {
    host: Url,
    authorization: HeaderValue,
    client: Client,
}

impl OnePasswordConnectSecretManager {
    /// Creates a reusable Connect client.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe host URL, empty/invalid token, or HTTP
    /// client construction failure.
    pub fn new(
        host: impl AsRef<str>,
        token: impl AsRef<str>,
    ) -> Result<Self, OnePasswordConnectConfigError> {
        let host =
            Url::parse(host.as_ref()).map_err(|_| OnePasswordConnectConfigError::InvalidHost)?;
        if !matches!(host.scheme(), "http" | "https")
            || host.host_str().is_none()
            || !host.username().is_empty()
            || host.password().is_some()
            || host.query().is_some()
            || host.fragment().is_some()
            || host.path() != "/"
        {
            return Err(OnePasswordConnectConfigError::InvalidHost);
        }
        if token.as_ref().is_empty() {
            return Err(OnePasswordConnectConfigError::InvalidToken);
        }
        let authorization = HeaderValue::from_str(&format!("Bearer {}", token.as_ref()))
            .map_err(|_| OnePasswordConnectConfigError::InvalidToken)?;
        let client = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .user_agent(concat!("nanocentaur/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(OnePasswordConnectConfigError::Client)?;
        Ok(Self {
            host,
            authorization,
            client,
        })
    }

    async fn fetch(&self, reference: &OpReference) -> Result<String, ConnectError> {
        let vault = self.vault(&reference.vault).await?;
        let item = self.item(&vault.id, &reference.item).await?;
        select_field(&item, reference)
    }

    async fn vault(&self, reference: &str) -> Result<ConnectVault, ConnectError> {
        if is_connect_id(reference) {
            return self
                .get_json(self.endpoint(&["v1", "vaults", reference])?)
                .await;
        }
        let mut endpoint = self.endpoint(&["v1", "vaults"])?;
        endpoint
            .query_pairs_mut()
            .append_pair("filter", &format!("title eq \"{reference}\""));
        exactly_one(self.get_json(endpoint).await?)
    }

    async fn item(&self, vault_id: &str, reference: &str) -> Result<ConnectItem, ConnectError> {
        if is_connect_id(reference) {
            return self
                .get_json(self.endpoint(&["v1", "vaults", vault_id, "items", reference])?)
                .await;
        }
        let mut endpoint = self.endpoint(&["v1", "vaults", vault_id, "items"])?;
        endpoint
            .query_pairs_mut()
            .append_pair("filter", &format!("title eq \"{reference}\""));
        let summary: ConnectItemSummary = exactly_one(self.get_json(endpoint).await?)?;
        self.get_json(self.endpoint(&["v1", "vaults", vault_id, "items", &summary.id])?)
            .await
    }

    fn endpoint(&self, segments: &[&str]) -> Result<Url, ConnectError> {
        let mut endpoint = self.host.clone();
        endpoint
            .path_segments_mut()
            .map_err(|()| ConnectError::InvalidResponse)?
            .extend(segments);
        Ok(endpoint)
    }

    async fn get_json<T>(&self, endpoint: Url) -> Result<T, ConnectError>
    where
        T: for<'de> Deserialize<'de>,
    {
        let response = self
            .client
            .get(endpoint)
            .header(AUTHORIZATION, self.authorization.clone())
            .header(ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| ConnectError::Unavailable)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(ConnectError::NotFound);
        }
        if !response.status().is_success() {
            return Err(ConnectError::Status(response.status().as_u16()));
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream
            .try_next()
            .await
            .map_err(|_| ConnectError::Unavailable)?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_CONNECT_RESPONSE_BYTES {
                return Err(ConnectError::ResponseTooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| ConnectError::InvalidResponse)
    }
}

#[async_trait]
impl SecretManager for OnePasswordConnectSecretManager {
    async fn resolve(&self, reference: &SecretRef) -> Result<String, SecretError> {
        let parsed =
            OpReference::parse(&reference.key).map_err(|()| SecretError::InvalidReference {
                provider: reference.provider.clone(),
                key: reference.key.clone(),
            })?;
        self.fetch(&parsed).await.map_err(|error| match error {
            ConnectError::NotFound => SecretError::NotFound {
                provider: reference.provider.clone(),
                key: reference.key.clone(),
            },
            ConnectError::Ambiguous => {
                SecretError::Provider("1Password Connect reference is ambiguous".to_owned())
            }
            ConnectError::EmptyValue => {
                SecretError::Provider("1Password Connect resolved an empty value".to_owned())
            }
            ConnectError::Status(status) => {
                SecretError::Provider(format!("1Password Connect returned HTTP {status}"))
            }
            ConnectError::Unavailable => {
                SecretError::Provider("1Password Connect is unavailable".to_owned())
            }
            ConnectError::ResponseTooLarge => {
                SecretError::Provider("1Password Connect response is too large".to_owned())
            }
            ConnectError::InvalidResponse => {
                SecretError::Provider("1Password Connect returned an invalid response".to_owned())
            }
        })
    }
}

#[derive(Debug, Error)]
pub enum OnePasswordConnectConfigError {
    #[error("1Password Connect host must be an HTTP(S) origin without credentials or a path")]
    InvalidHost,
    #[error("1Password Connect token is empty or invalid")]
    InvalidToken,
    #[error("1Password Connect HTTP client could not be built")]
    Client(#[source] reqwest::Error),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct OpReference {
    vault: String,
    item: String,
    section: Option<String>,
    field: String,
}

impl OpReference {
    pub(super) fn parse(value: &str) -> Result<Self, ()> {
        let rest = value.strip_prefix("op://").ok_or(())?;
        let segments = rest.split('/').collect::<Vec<_>>();
        if !matches!(segments.len(), 3 | 4)
            || !segments.iter().all(|segment| valid_segment(segment))
        {
            return Err(());
        }
        Ok(if segments.len() == 3 {
            Self {
                vault: segments[0].to_owned(),
                item: segments[1].to_owned(),
                section: None,
                field: segments[2].to_owned(),
            }
        } else {
            Self {
                vault: segments[0].to_owned(),
                item: segments[1].to_owned(),
                section: Some(segments[2].to_owned()),
                field: segments[3].to_owned(),
            }
        })
    }
}

fn valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= MAX_REFERENCE_SEGMENT_BYTES
        && segment.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || character == ' '
                || matches!(character, '-' | '_' | '.')
        })
}

fn is_connect_id(value: &str) -> bool {
    value.len() == 26
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

fn exactly_one<T>(mut values: Vec<T>) -> Result<T, ConnectError> {
    if values.len() > 1 {
        return Err(ConnectError::Ambiguous);
    }
    values.pop().ok_or(ConnectError::NotFound)
}

fn select_field(item: &ConnectItem, reference: &OpReference) -> Result<String, ConnectError> {
    let field =
        item.fields
            .iter()
            .find(|field| {
                (field.id == reference.field || field.label == reference.field)
                    && reference.section.as_deref().is_none_or(|section| {
                        section_matches(item, field.section.as_ref(), section)
                    })
            })
            .ok_or(ConnectError::NotFound)?;
    field
        .value
        .clone()
        .filter(|value| !value.is_empty())
        .ok_or(ConnectError::EmptyValue)
}

fn section_matches(
    item: &ConnectItem,
    field_section: Option<&ConnectSection>,
    reference: &str,
) -> bool {
    let Some(field_section) = field_section else {
        return false;
    };
    if field_section.id == reference || field_section.label.as_deref() == Some(reference) {
        return true;
    }
    item.sections.iter().any(|section| {
        section.id == field_section.id
            && (section.id == reference || section.label.as_deref() == Some(reference))
    })
}

#[derive(Deserialize)]
struct ConnectVault {
    id: String,
}

#[derive(Deserialize)]
struct ConnectItemSummary {
    id: String,
}

#[derive(Deserialize)]
struct ConnectItem {
    #[serde(default)]
    sections: Vec<ConnectSection>,
    #[serde(default)]
    fields: Vec<ConnectField>,
}

#[derive(Deserialize)]
struct ConnectField {
    id: String,
    label: String,
    value: Option<String>,
    section: Option<ConnectSection>,
}

#[derive(Deserialize)]
struct ConnectSection {
    id: String,
    label: Option<String>,
}

enum ConnectError {
    NotFound,
    Ambiguous,
    EmptyValue,
    Status(u16),
    Unavailable,
    ResponseTooLarge,
    InvalidResponse,
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, RwLock};

    use axum::{
        Json, Router,
        extract::{Query, State},
        http::{HeaderMap, StatusCode},
        routing::get,
    };
    use serde::Deserialize;
    use serde_json::{Value, json};

    use super::*;

    const VAULT_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
    const ITEM_ID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[derive(Clone)]
    struct ConnectState {
        requests: Arc<Mutex<Vec<String>>>,
        value: Arc<RwLock<String>>,
    }

    #[derive(Deserialize)]
    struct Filter {
        filter: String,
    }

    #[tokio::test]
    async fn resolves_named_section_fields_with_bearer_auth_and_no_cache() {
        let state = ConnectState {
            requests: Arc::new(Mutex::new(Vec::new())),
            value: Arc::new(RwLock::new("first-secret".to_owned())),
        };
        let app = Router::new()
            .route("/v1/vaults", get(vaults))
            .route(&format!("/v1/vaults/{VAULT_ID}"), get(vault))
            .route(&format!("/v1/vaults/{VAULT_ID}/items"), get(items))
            .route(&format!("/v1/vaults/{VAULT_ID}/items/{ITEM_ID}"), get(item))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let manager =
            OnePasswordConnectSecretManager::new(format!("http://{address}"), "connect-token")
                .unwrap();
        let reference = SecretRef {
            provider: "1password_connect".to_owned(),
            key: "op://Engineering/OpenAI/api/api_key".to_owned(),
        };

        assert_eq!(manager.resolve(&reference).await.unwrap(), "first-secret");
        *state.value.write().unwrap() = "rotated-secret".to_owned();
        assert_eq!(manager.resolve(&reference).await.unwrap(), "rotated-secret");
        let id_reference = SecretRef {
            provider: "1password_connect".to_owned(),
            key: format!("op://{VAULT_ID}/{ITEM_ID}/api_key"),
        };
        assert_eq!(
            manager.resolve(&id_reference).await.unwrap(),
            "rotated-secret"
        );
        assert_eq!(
            state.requests.lock().unwrap().as_slice(),
            [
                "vaults:title eq \"Engineering\"",
                "items:title eq \"OpenAI\"",
                "item",
                "vaults:title eq \"Engineering\"",
                "items:title eq \"OpenAI\"",
                "item",
                "vault",
                "item",
            ]
        );
        server.abort();
    }

    #[test]
    fn rejects_non_field_references_and_unsafe_segments() {
        for reference in [
            "Engineering/OpenAI/credential",
            "op://Engineering/OpenAI",
            "op://Engineering/OpenAI/section/field/extra",
            "op://Engineering/OpenAI/bad?attribute=value",
            "op://Engineering/OpenAI/bad%2Ffield",
            "op://Engineering/OpenAI/bad\tfield",
        ] {
            assert!(OpReference::parse(reference).is_err(), "{reference}");
        }
    }

    async fn vaults(
        State(state): State<ConnectState>,
        Query(filter): Query<Filter>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<Value>) {
        record(&state, &headers, format!("vaults:{}", filter.filter));
        (
            StatusCode::OK,
            Json(json!([{"id": VAULT_ID, "name": "Engineering"}])),
        )
    }

    async fn items(
        State(state): State<ConnectState>,
        Query(filter): Query<Filter>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<Value>) {
        record(&state, &headers, format!("items:{}", filter.filter));
        (
            StatusCode::OK,
            Json(json!([{"id": ITEM_ID, "title": "OpenAI"}])),
        )
    }

    async fn vault(
        State(state): State<ConnectState>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<Value>) {
        record(&state, &headers, "vault".to_owned());
        (
            StatusCode::OK,
            Json(json!({"id": VAULT_ID, "name": "Engineering"})),
        )
    }

    async fn item(
        State(state): State<ConnectState>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<Value>) {
        record(&state, &headers, "item".to_owned());
        let value = state.value.read().unwrap().clone();
        (
            StatusCode::OK,
            Json(json!({
                "id": ITEM_ID,
                "sections": [{"id": "section-id", "label": "api"}],
                "fields": [{
                    "id": "field-id",
                    "label": "api_key",
                    "value": value,
                    "section": {"id": "section-id"}
                }]
            })),
        )
    }

    fn record(state: &ConnectState, headers: &HeaderMap, request: String) {
        assert_eq!(headers.get(AUTHORIZATION).unwrap(), "Bearer connect-token");
        state.requests.lock().unwrap().push(request);
    }
}

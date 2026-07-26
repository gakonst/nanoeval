#![allow(clippy::missing_errors_doc, clippy::needless_pass_by_value)]

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{Mutex, MutexGuard},
};

use axum::http::HeaderMap;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

use crate::secrets::validate_secret;
use crate::{
    AgentCapabilities, CapabilityName, CreateSecret, PatchSecret, SecretDelivery,
    SecretGuestConfig, SecretRef, SecretRequestRule, SecretView,
};

const MAX_CONTEXT_KEY_BYTES: usize = 512;
const MAX_INSTRUCTIONS_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    None,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl From<ReasoningEffort> for nanocodex::Thinking {
    fn from(value: ReasoningEffort) -> Self {
        match value {
            ReasoningEffort::None => Self::None,
            ReasoningEffort::Low => Self::Low,
            ReasoningEffort::Medium => Self::Medium,
            ReasoningEffort::High => Self::High,
            ReasoningEffort::Xhigh => Self::Xhigh,
            ReasoningEffort::Max => Self::Max,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PrincipalMetadata {
    pub labels: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct AuthenticatedClient {
    pub id: String,
    pub default_principal_id: String,
}

#[derive(Clone, Debug)]
pub struct EffectivePrincipal {
    pub id: String,
    pub agent_config: AgentConfig,
    pub permissions: AgentCapabilities,
    /// Changes whenever secret configuration or grants may affect this principal.
    pub secret_revision: u64,
}

impl EffectivePrincipal {
    #[must_use]
    pub fn allows(&self, permission: &str) -> bool {
        self.permissions.contains(permission)
    }
}

#[derive(Clone, Debug)]
pub struct AgentIdentity {
    pub id: String,
    pub owner_client_id: String,
    pub context_key: Option<String>,
    pub principal: EffectivePrincipal,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateApiClient {
    pub id: Option<String>,
    pub name: String,
    pub api_key: String,
    pub default_principal_id: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ApiClientView {
    pub id: String,
    pub name: String,
    pub default_principal_id: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PatchApiClient {
    pub name: Option<String>,
    pub default_principal_id: Option<String>,
    pub enabled: Option<bool>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateApiKey {
    pub api_key: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ApiKeyView {
    pub id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatePrincipal {
    pub id: Option<String>,
    pub name: String,
    pub external_id: Option<String>,
    #[serde(default)]
    pub metadata: PrincipalMetadata,
    #[serde(default)]
    pub agent_config: AgentConfig,
}

#[derive(Clone, Debug, Serialize)]
pub struct PrincipalView {
    pub id: String,
    pub name: String,
    pub external_id: Option<String>,
    pub enabled: bool,
    pub metadata: PrincipalMetadata,
    pub agent_config: AgentConfig,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PatchPrincipal {
    pub name: Option<String>,
    pub external_id: Option<Option<String>>,
    pub enabled: Option<bool>,
    pub metadata: Option<PrincipalMetadata>,
    pub agent_config: Option<AgentConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateContextBinding {
    pub api_client_id: String,
    pub context_key: String,
    pub principal_id: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ContextBindingView {
    pub id: String,
    pub api_client_id: String,
    pub context_key: String,
    pub principal_id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PatchContextBinding {
    pub context_key: Option<String>,
    pub principal_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveContext {
    pub api_client_id: String,
    pub context_key: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ResolvedContextView {
    pub api_client_id: String,
    pub context_key: String,
    pub principal_id: String,
    pub source: &'static str,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRole {
    pub id: Option<String>,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RoleView {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PatchRole {
    pub name: Option<String>,
    pub description: Option<Option<String>>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatePermission {
    pub id: Option<String>,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PermissionView {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

/// `SQLite` is the durable source of truth for identities, context resolution,
/// reusable roles, direct grants, and agent ownership.
pub struct PolicyStore {
    connection: Mutex<Connection>,
}

impl PolicyStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, PolicyError> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }
        Self::from_connection(Connection::open(path)?)
    }

    pub fn in_memory() -> Result<Self, PolicyError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    #[allow(clippy::too_many_lines)]
    fn from_connection(connection: Connection) -> Result<Self, PolicyError> {
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(
            r"
            CREATE TABLE IF NOT EXISTS principals (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                external_id TEXT UNIQUE,
                enabled INTEGER NOT NULL DEFAULT 1,
                metadata_json TEXT NOT NULL DEFAULT '{}',
                agent_config_json TEXT NOT NULL DEFAULT '{}',
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS api_clients (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                default_principal_id TEXT NOT NULL REFERENCES principals(id),
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS api_client_keys (
                id TEXT PRIMARY KEY,
                api_client_id TEXT NOT NULL REFERENCES api_clients(id) ON DELETE CASCADE,
                key_hash BLOB NOT NULL UNIQUE,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS context_bindings (
                id TEXT PRIMARY KEY,
                api_client_id TEXT NOT NULL REFERENCES api_clients(id) ON DELETE CASCADE,
                context_key TEXT NOT NULL,
                principal_id TEXT NOT NULL REFERENCES principals(id),
                created_at TEXT NOT NULL,
                UNIQUE(api_client_id, context_key)
            );

            CREATE TABLE IF NOT EXISTS roles (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                description TEXT
            );

            CREATE TABLE IF NOT EXISTS permissions (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                description TEXT
            );

            CREATE TABLE IF NOT EXISTS principal_roles (
                principal_id TEXT NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
                role_id TEXT NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
                PRIMARY KEY(principal_id, role_id)
            );

            CREATE TABLE IF NOT EXISTS role_permissions (
                role_id TEXT NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
                permission_id TEXT NOT NULL REFERENCES permissions(id) ON DELETE CASCADE,
                PRIMARY KEY(role_id, permission_id)
            );

            CREATE TABLE IF NOT EXISTS principal_permissions (
                principal_id TEXT NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
                permission_id TEXT NOT NULL REFERENCES permissions(id) ON DELETE CASCADE,
                PRIMARY KEY(principal_id, permission_id)
            );

            CREATE TABLE IF NOT EXISTS secrets (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                source_json TEXT NOT NULL,
                upstream TEXT NOT NULL,
                rules_json TEXT NOT NULL,
                delivery_json TEXT NOT NULL,
                guest_json TEXT NOT NULL,
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS principal_secrets (
                principal_id TEXT NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
                secret_id TEXT NOT NULL REFERENCES secrets(id) ON DELETE CASCADE,
                PRIMARY KEY(principal_id, secret_id)
            );

            CREATE TABLE IF NOT EXISTS role_secrets (
                role_id TEXT NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
                secret_id TEXT NOT NULL REFERENCES secrets(id) ON DELETE CASCADE,
                PRIMARY KEY(role_id, secret_id)
            );

            CREATE TABLE IF NOT EXISTS policy_state (
                key TEXT PRIMARY KEY,
                value INTEGER NOT NULL
            );

            INSERT OR IGNORE INTO policy_state (key, value)
            VALUES ('secret_revision', 0);

            CREATE TABLE IF NOT EXISTS agents (
                id TEXT PRIMARY KEY,
                owner_client_id TEXT NOT NULL REFERENCES api_clients(id),
                context_key TEXT,
                principal_id TEXT NOT NULL REFERENCES principals(id),
                agent_config_json TEXT NOT NULL,
                created_at TEXT NOT NULL,
                UNIQUE(owner_client_id, context_key)
            );

            CREATE TABLE IF NOT EXISTS audit_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                actor TEXT NOT NULL,
                action TEXT NOT NULL,
                target_type TEXT NOT NULL,
                target_id TEXT NOT NULL,
                data_json TEXT NOT NULL DEFAULT '{}',
                created_at TEXT NOT NULL
            );
            ",
        )?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn bootstrap(
        &self,
        api_client_id: &str,
        api_client_name: &str,
        api_key: &str,
        principal_id: &str,
        permissions: impl IntoIterator<Item = CapabilityName>,
    ) -> Result<(), PolicyError> {
        validate_nonempty(api_client_id, "api client id")?;
        validate_nonempty(api_key, "API key")?;
        let now = now();
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT OR IGNORE INTO principals
             (id, name, metadata_json, agent_config_json, created_at)
             VALUES (?1, ?2, '{}', '{}', ?3)",
            params![principal_id, principal_id, now],
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO api_clients
             (id, name, default_principal_id, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![api_client_id, api_client_name, principal_id, now],
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO api_client_keys
             (id, api_client_id, key_hash, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                Uuid::new_v4().to_string(),
                api_client_id,
                hash_key(api_key).to_vec(),
                now
            ],
        )?;
        for permission in standard_agent_permissions().into_iter().chain(permissions) {
            grant_direct(&transaction, principal_id, permission.as_str())?;
        }
        audit(
            &transaction,
            "bootstrap",
            "bootstrap",
            "api_client",
            api_client_id,
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn authenticate(&self, headers: &HeaderMap) -> Result<AuthenticatedClient, PolicyError> {
        let token = headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .ok_or(PolicyError::Unauthenticated)?;
        let digest = hash_key(token);
        let connection = self.lock()?;
        connection
            .query_row(
                "SELECT c.id, c.default_principal_id
                 FROM api_client_keys k
                 JOIN api_clients c ON c.id = k.api_client_id
                 WHERE k.key_hash = ?1 AND c.enabled = 1",
                params![digest.to_vec()],
                |row| {
                    Ok(AuthenticatedClient {
                        id: row.get(0)?,
                        default_principal_id: row.get(1)?,
                    })
                },
            )
            .optional()?
            .ok_or(PolicyError::Unauthenticated)
    }

    pub fn create_or_resolve_agent(
        &self,
        client: &AuthenticatedClient,
        context_key: Option<&str>,
    ) -> Result<(AgentIdentity, bool), PolicyError> {
        validate_context_key(context_key)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        if let Some(context_key) = context_key
            && let Some(agent_id) = transaction
                .query_row(
                    "SELECT id FROM agents
                     WHERE owner_client_id = ?1 AND context_key = ?2",
                    params![client.id, context_key],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
        {
            let identity = agent_identity(&transaction, &client.id, &agent_id)?;
            require(&identity.principal, "agent.read")?;
            transaction.commit()?;
            return Ok((identity, false));
        }

        let principal_id = resolve_principal_id(&transaction, client, context_key)?;
        let config = principal_config(&transaction, &principal_id)?;
        let principal = EffectivePrincipal {
            id: principal_id.clone(),
            agent_config: config.clone(),
            permissions: effective_capabilities(&transaction, &principal_id)?,
            secret_revision: secret_revision(&transaction)?,
        };
        require(&principal, "agent.new")?;
        let id = Uuid::new_v4().to_string();
        let created_at = now();
        transaction.execute(
            "INSERT INTO agents
             (id, owner_client_id, context_key, principal_id, agent_config_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                client.id,
                context_key,
                principal_id,
                serde_json::to_string(&config)?,
                created_at
            ],
        )?;
        audit(&transaction, &client.id, "agent.create", "agent", &id)?;
        let identity = agent_identity(&transaction, &client.id, &id)?;
        transaction.commit()?;
        Ok((identity, true))
    }

    pub fn agent(
        &self,
        client: &AuthenticatedClient,
        agent_id: &str,
    ) -> Result<AgentIdentity, PolicyError> {
        let connection = self.lock()?;
        agent_identity(&connection, &client.id, agent_id)
    }

    pub fn fork_agent(
        &self,
        client: &AuthenticatedClient,
        source_agent_id: &str,
    ) -> Result<AgentIdentity, PolicyError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let source = agent_identity(&transaction, &client.id, source_agent_id)?;
        require(&source.principal, "agent.fork")?;
        let id = Uuid::new_v4().to_string();
        let created_at = now();
        transaction.execute(
            "INSERT INTO agents
             (id, owner_client_id, context_key, principal_id, agent_config_json, created_at)
             SELECT ?1, owner_client_id, NULL, principal_id, agent_config_json, ?2
             FROM agents WHERE id = ?3 AND owner_client_id = ?4",
            params![id, created_at, source_agent_id, client.id],
        )?;
        audit(&transaction, &client.id, "agent.fork", "agent", &id)?;
        let identity = agent_identity(&transaction, &client.id, &id)?;
        transaction.commit()?;
        Ok(identity)
    }

    pub fn delete_agent(
        &self,
        client: &AuthenticatedClient,
        agent_id: &str,
    ) -> Result<(), PolicyError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let identity = agent_identity(&transaction, &client.id, agent_id)?;
        require(&identity.principal, "agent.delete")?;
        transaction.execute(
            "DELETE FROM agents WHERE id = ?1 AND owner_client_id = ?2",
            params![agent_id, client.id],
        )?;
        audit(&transaction, &client.id, "agent.delete", "agent", agent_id)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn create_api_client(
        &self,
        request: CreateApiClient,
    ) -> Result<ApiClientView, PolicyError> {
        validate_nonempty(&request.name, "name")?;
        validate_nonempty(&request.api_key, "API key")?;
        let id = request.id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let created_at = now();
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO api_clients
             (id, name, default_principal_id, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, request.name, request.default_principal_id, created_at],
        )?;
        transaction.execute(
            "INSERT INTO api_client_keys
             (id, api_client_id, key_hash, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                Uuid::new_v4().to_string(),
                id,
                hash_key(&request.api_key).to_vec(),
                created_at
            ],
        )?;
        audit(
            &transaction,
            "admin",
            "api_client.create",
            "api_client",
            &id,
        )?;
        transaction.commit()?;
        self.api_client(&id)
    }

    pub fn api_clients(&self) -> Result<Vec<ApiClientView>, PolicyError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT id, name, default_principal_id, enabled, created_at
             FROM api_clients ORDER BY created_at, id",
        )?;
        collect_rows(statement.query_map([], api_client_row)?)
    }

    pub fn api_client(&self, id: &str) -> Result<ApiClientView, PolicyError> {
        let connection = self.lock()?;
        connection
            .query_row(
                "SELECT id, name, default_principal_id, enabled, created_at
                 FROM api_clients WHERE id = ?1",
                params![id],
                api_client_row,
            )
            .optional()?
            .ok_or(PolicyError::NotFound)
    }

    pub fn patch_api_client(
        &self,
        id: &str,
        patch: PatchApiClient,
    ) -> Result<ApiClientView, PolicyError> {
        let connection = self.lock()?;
        let current = query_api_client(&connection, id)?;
        connection.execute(
            "UPDATE api_clients SET name = ?2, default_principal_id = ?3, enabled = ?4
             WHERE id = ?1",
            params![
                id,
                patch.name.unwrap_or(current.name),
                patch
                    .default_principal_id
                    .unwrap_or(current.default_principal_id),
                patch.enabled.unwrap_or(current.enabled)
            ],
        )?;
        drop(connection);
        self.api_client(id)
    }

    pub fn disable_api_client(&self, id: &str) -> Result<(), PolicyError> {
        changed(self.lock()?.execute(
            "UPDATE api_clients SET enabled = 0 WHERE id = ?1",
            params![id],
        )?)
    }

    pub fn add_api_key(
        &self,
        client_id: &str,
        request: CreateApiKey,
    ) -> Result<ApiKeyView, PolicyError> {
        validate_nonempty(&request.api_key, "API key")?;
        let view = ApiKeyView {
            id: Uuid::new_v4().to_string(),
            created_at: Utc::now(),
        };
        self.lock()?.execute(
            "INSERT INTO api_client_keys (id, api_client_id, key_hash, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                view.id,
                client_id,
                hash_key(&request.api_key).to_vec(),
                view.created_at.to_rfc3339()
            ],
        )?;
        Ok(view)
    }

    pub fn delete_api_key(&self, client_id: &str, key_id: &str) -> Result<(), PolicyError> {
        changed(self.lock()?.execute(
            "DELETE FROM api_client_keys WHERE id = ?1 AND api_client_id = ?2",
            params![key_id, client_id],
        )?)
    }

    pub fn create_principal(&self, request: CreatePrincipal) -> Result<PrincipalView, PolicyError> {
        validate_nonempty(&request.name, "name")?;
        validate_agent_config(&request.agent_config)?;
        let id = request.id.unwrap_or_else(|| Uuid::new_v4().to_string());
        self.lock()?.execute(
            "INSERT INTO principals
             (id, name, external_id, metadata_json, agent_config_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                request.name,
                request.external_id,
                serde_json::to_string(&request.metadata)?,
                serde_json::to_string(&request.agent_config)?,
                now()
            ],
        )?;
        self.principal(&id)
    }

    pub fn principals(&self) -> Result<Vec<PrincipalView>, PolicyError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT id, name, external_id, enabled, metadata_json,
                    agent_config_json, created_at
             FROM principals ORDER BY created_at, id",
        )?;
        collect_rows(statement.query_map([], principal_row)?)
    }

    pub fn principal(&self, id: &str) -> Result<PrincipalView, PolicyError> {
        let connection = self.lock()?;
        query_principal(&connection, id)
    }

    pub fn patch_principal(
        &self,
        id: &str,
        patch: PatchPrincipal,
    ) -> Result<PrincipalView, PolicyError> {
        let connection = self.lock()?;
        let current = query_principal(&connection, id)?;
        if let Some(config) = &patch.agent_config {
            validate_agent_config(config)?;
        }
        connection.execute(
            "UPDATE principals SET name = ?2, external_id = ?3, enabled = ?4,
             metadata_json = ?5, agent_config_json = ?6 WHERE id = ?1",
            params![
                id,
                patch.name.unwrap_or(current.name),
                patch.external_id.unwrap_or(current.external_id),
                patch.enabled.unwrap_or(current.enabled),
                serde_json::to_string(&patch.metadata.unwrap_or(current.metadata))?,
                serde_json::to_string(&patch.agent_config.unwrap_or(current.agent_config))?
            ],
        )?;
        drop(connection);
        self.principal(id)
    }

    pub fn disable_principal(&self, id: &str) -> Result<(), PolicyError> {
        changed(self.lock()?.execute(
            "UPDATE principals SET enabled = 0 WHERE id = ?1",
            params![id],
        )?)
    }

    pub fn create_context_binding(
        &self,
        request: CreateContextBinding,
    ) -> Result<ContextBindingView, PolicyError> {
        validate_context_key(Some(&request.context_key))?;
        let id = Uuid::new_v4().to_string();
        self.lock()?.execute(
            "INSERT INTO context_bindings
             (id, api_client_id, context_key, principal_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id,
                request.api_client_id,
                request.context_key,
                request.principal_id,
                now()
            ],
        )?;
        self.context_binding(&id)
    }

    pub fn context_bindings(&self) -> Result<Vec<ContextBindingView>, PolicyError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT id, api_client_id, context_key, principal_id, created_at
             FROM context_bindings ORDER BY created_at, id",
        )?;
        collect_rows(statement.query_map([], context_binding_row)?)
    }

    pub fn context_binding(&self, id: &str) -> Result<ContextBindingView, PolicyError> {
        self.lock()?
            .query_row(
                "SELECT id, api_client_id, context_key, principal_id, created_at
                 FROM context_bindings WHERE id = ?1",
                params![id],
                context_binding_row,
            )
            .optional()?
            .ok_or(PolicyError::NotFound)
    }

    pub fn patch_context_binding(
        &self,
        id: &str,
        patch: PatchContextBinding,
    ) -> Result<ContextBindingView, PolicyError> {
        let current = self.context_binding(id)?;
        let context_key = patch.context_key.unwrap_or(current.context_key);
        validate_context_key(Some(&context_key))?;
        self.lock()?.execute(
            "UPDATE context_bindings SET context_key = ?2, principal_id = ?3 WHERE id = ?1",
            params![
                id,
                context_key,
                patch.principal_id.unwrap_or(current.principal_id)
            ],
        )?;
        self.context_binding(id)
    }

    pub fn delete_context_binding(&self, id: &str) -> Result<(), PolicyError> {
        changed(
            self.lock()?
                .execute("DELETE FROM context_bindings WHERE id = ?1", params![id])?,
        )
    }

    pub fn resolve_context(
        &self,
        request: ResolveContext,
    ) -> Result<ResolvedContextView, PolicyError> {
        validate_context_key(Some(&request.context_key))?;
        let connection = self.lock()?;
        let bound = connection
            .query_row(
                "SELECT principal_id FROM context_bindings
                 WHERE api_client_id = ?1 AND context_key = ?2",
                params![request.api_client_id, request.context_key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let (principal_id, source) = match bound {
            Some(id) => (id, "binding"),
            None => (
                connection
                    .query_row(
                        "SELECT default_principal_id FROM api_clients
                         WHERE id = ?1 AND enabled = 1",
                        params![request.api_client_id],
                        |row| row.get(0),
                    )
                    .optional()?
                    .ok_or(PolicyError::NotFound)?,
                "default",
            ),
        };
        Ok(ResolvedContextView {
            api_client_id: request.api_client_id,
            context_key: request.context_key,
            principal_id,
            source,
        })
    }

    pub fn create_role(&self, request: CreateRole) -> Result<RoleView, PolicyError> {
        validate_nonempty(&request.name, "name")?;
        let id = request.id.unwrap_or_else(|| Uuid::new_v4().to_string());
        self.lock()?.execute(
            "INSERT INTO roles (id, name, description) VALUES (?1, ?2, ?3)",
            params![id, request.name, request.description],
        )?;
        self.role(&id)
    }

    pub fn roles(&self) -> Result<Vec<RoleView>, PolicyError> {
        let connection = self.lock()?;
        let mut statement =
            connection.prepare("SELECT id, name, description FROM roles ORDER BY name, id")?;
        collect_rows(statement.query_map([], role_row)?)
    }

    pub fn role(&self, id: &str) -> Result<RoleView, PolicyError> {
        self.lock()?
            .query_row(
                "SELECT id, name, description FROM roles WHERE id = ?1",
                params![id],
                role_row,
            )
            .optional()?
            .ok_or(PolicyError::NotFound)
    }

    pub fn patch_role(&self, id: &str, patch: PatchRole) -> Result<RoleView, PolicyError> {
        let current = self.role(id)?;
        self.lock()?.execute(
            "UPDATE roles SET name = ?2, description = ?3 WHERE id = ?1",
            params![
                id,
                patch.name.unwrap_or(current.name),
                patch.description.unwrap_or(current.description)
            ],
        )?;
        self.role(id)
    }

    pub fn delete_role(&self, id: &str) -> Result<(), PolicyError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        changed(transaction.execute("DELETE FROM roles WHERE id = ?1", params![id])?)?;
        bump_secret_revision(&transaction)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn create_permission(
        &self,
        request: CreatePermission,
    ) -> Result<PermissionView, PolicyError> {
        CapabilityName::new(request.name.clone())
            .map_err(|_| PolicyError::Invalid("invalid permission name"))?;
        let id = request.id.unwrap_or_else(|| Uuid::new_v4().to_string());
        self.lock()?.execute(
            "INSERT INTO permissions (id, name, description) VALUES (?1, ?2, ?3)",
            params![id, request.name, request.description],
        )?;
        self.permission(&id)
    }

    pub fn permissions(&self) -> Result<Vec<PermissionView>, PolicyError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare("SELECT id, name, description FROM permissions ORDER BY name, id")?;
        collect_rows(statement.query_map([], permission_row)?)
    }

    pub fn permission(&self, id: &str) -> Result<PermissionView, PolicyError> {
        self.lock()?
            .query_row(
                "SELECT id, name, description FROM permissions WHERE id = ?1",
                params![id],
                permission_row,
            )
            .optional()?
            .ok_or(PolicyError::NotFound)
    }

    pub fn delete_permission(&self, id: &str) -> Result<(), PolicyError> {
        changed(
            self.lock()?
                .execute("DELETE FROM permissions WHERE id = ?1", params![id])?,
        )
    }

    pub fn set_principal_role(
        &self,
        principal_id: &str,
        role_id: &str,
        present: bool,
    ) -> Result<(), PolicyError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        set_join(
            &transaction,
            "principal_roles",
            "principal_id",
            principal_id,
            "role_id",
            role_id,
            present,
        )?;
        bump_secret_revision(&transaction)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn principal_roles(&self, principal_id: &str) -> Result<Vec<RoleView>, PolicyError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT r.id, r.name, r.description FROM roles r
             JOIN principal_roles pr ON pr.role_id = r.id
             WHERE pr.principal_id = ?1 ORDER BY r.name, r.id",
        )?;
        collect_rows(statement.query_map(params![principal_id], role_row)?)
    }

    pub fn set_role_permission(
        &self,
        role_id: &str,
        permission_id: &str,
        present: bool,
    ) -> Result<(), PolicyError> {
        set_join(
            &*self.lock()?,
            "role_permissions",
            "role_id",
            role_id,
            "permission_id",
            permission_id,
            present,
        )
    }

    pub fn role_permissions(&self, role_id: &str) -> Result<Vec<PermissionView>, PolicyError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT p.id, p.name, p.description FROM permissions p
             JOIN role_permissions rp ON rp.permission_id = p.id
             WHERE rp.role_id = ?1 ORDER BY p.name, p.id",
        )?;
        collect_rows(statement.query_map(params![role_id], permission_row)?)
    }

    pub fn set_principal_permission(
        &self,
        principal_id: &str,
        permission_id: &str,
        present: bool,
    ) -> Result<(), PolicyError> {
        set_join(
            &*self.lock()?,
            "principal_permissions",
            "principal_id",
            principal_id,
            "permission_id",
            permission_id,
            present,
        )
    }

    pub fn principal_permissions(
        &self,
        principal_id: &str,
    ) -> Result<Vec<PermissionView>, PolicyError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT p.id, p.name, p.description FROM permissions p
             JOIN principal_permissions pp ON pp.permission_id = p.id
             WHERE pp.principal_id = ?1 ORDER BY p.name, p.id",
        )?;
        collect_rows(statement.query_map(params![principal_id], permission_row)?)
    }

    pub fn effective_permissions(
        &self,
        principal_id: &str,
    ) -> Result<Vec<PermissionView>, PolicyError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT DISTINCT p.id, p.name, p.description
             FROM permissions p
             WHERE p.id IN (
                 SELECT permission_id FROM principal_permissions WHERE principal_id = ?1
                 UNION
                 SELECT rp.permission_id FROM role_permissions rp
                 JOIN principal_roles pr ON pr.role_id = rp.role_id
                 WHERE pr.principal_id = ?1
             )
             ORDER BY p.name, p.id",
        )?;
        collect_rows(statement.query_map(params![principal_id], permission_row)?)
    }

    pub fn create_secret(&self, request: CreateSecret) -> Result<SecretView, PolicyError> {
        validate_secret(&request)
            .map_err(|_| PolicyError::Invalid("invalid secret configuration"))?;
        let id = request
            .id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        validate_nonempty(&id, "secret id")?;
        let timestamp = now();
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO secrets
             (id, name, source_json, upstream, rules_json, delivery_json, guest_json,
              enabled, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?8)",
            params![
                id,
                request.name,
                serde_json::to_string(&request.source)?,
                request.upstream,
                serde_json::to_string(&request.rules)?,
                serde_json::to_string(&request.delivery)?,
                serde_json::to_string(&request.guest)?,
                timestamp,
            ],
        )?;
        audit(&transaction, "admin", "secret.create", "secret", &id)?;
        bump_secret_revision(&transaction)?;
        let secret = query_secret(&transaction, &id)?;
        transaction.commit()?;
        Ok(secret)
    }

    pub fn secrets(&self) -> Result<Vec<SecretView>, PolicyError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT id, name, source_json, upstream, rules_json, delivery_json,
                    guest_json, enabled, created_at, updated_at
             FROM secrets ORDER BY name, id",
        )?;
        collect_rows(statement.query_map([], secret_row)?)
    }

    pub fn secret(&self, id: &str) -> Result<SecretView, PolicyError> {
        let connection = self.lock()?;
        query_secret(&connection, id)
    }

    pub fn patch_secret(&self, id: &str, patch: PatchSecret) -> Result<SecretView, PolicyError> {
        let current = self.secret(id)?;
        let merged = CreateSecret {
            id: Some(id.to_owned()),
            name: patch.name.unwrap_or(current.name),
            source: patch.source.unwrap_or(current.source),
            upstream: patch.upstream.unwrap_or(current.upstream),
            rules: patch.rules.unwrap_or(current.rules),
            delivery: patch.delivery.unwrap_or(current.delivery),
            guest: patch.guest.unwrap_or(current.guest),
        };
        validate_secret(&merged)
            .map_err(|_| PolicyError::Invalid("invalid secret configuration"))?;
        let enabled = patch.enabled.unwrap_or(current.enabled);
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "UPDATE secrets
             SET name = ?2, source_json = ?3, upstream = ?4, rules_json = ?5,
                 delivery_json = ?6, guest_json = ?7, enabled = ?8, updated_at = ?9
             WHERE id = ?1",
            params![
                id,
                merged.name,
                serde_json::to_string(&merged.source)?,
                merged.upstream,
                serde_json::to_string(&merged.rules)?,
                serde_json::to_string(&merged.delivery)?,
                serde_json::to_string(&merged.guest)?,
                enabled,
                now(),
            ],
        )?;
        audit(&transaction, "admin", "secret.update", "secret", id)?;
        bump_secret_revision(&transaction)?;
        let secret = query_secret(&transaction, id)?;
        transaction.commit()?;
        Ok(secret)
    }

    pub fn delete_secret(&self, id: &str) -> Result<(), PolicyError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        changed(transaction.execute("DELETE FROM secrets WHERE id = ?1", params![id])?)?;
        audit(&transaction, "admin", "secret.delete", "secret", id)?;
        bump_secret_revision(&transaction)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn set_principal_secret(
        &self,
        principal_id: &str,
        secret_id: &str,
        present: bool,
    ) -> Result<(), PolicyError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        set_join(
            &transaction,
            "principal_secrets",
            "principal_id",
            principal_id,
            "secret_id",
            secret_id,
            present,
        )?;
        audit(
            &transaction,
            "admin",
            if present {
                "principal_secret.grant"
            } else {
                "principal_secret.revoke"
            },
            "secret",
            secret_id,
        )?;
        bump_secret_revision(&transaction)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn principal_secrets(&self, principal_id: &str) -> Result<Vec<SecretView>, PolicyError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT s.id, s.name, s.source_json, s.upstream, s.rules_json,
                    s.delivery_json, s.guest_json, s.enabled, s.created_at, s.updated_at
             FROM secrets s
             JOIN principal_secrets ps ON ps.secret_id = s.id
             WHERE ps.principal_id = ?1
             ORDER BY s.name, s.id",
        )?;
        collect_rows(statement.query_map(params![principal_id], secret_row)?)
    }

    pub fn set_role_secret(
        &self,
        role_id: &str,
        secret_id: &str,
        present: bool,
    ) -> Result<(), PolicyError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        set_join(
            &transaction,
            "role_secrets",
            "role_id",
            role_id,
            "secret_id",
            secret_id,
            present,
        )?;
        audit(
            &transaction,
            "admin",
            if present {
                "role_secret.grant"
            } else {
                "role_secret.revoke"
            },
            "secret",
            secret_id,
        )?;
        bump_secret_revision(&transaction)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn role_secrets(&self, role_id: &str) -> Result<Vec<SecretView>, PolicyError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT s.id, s.name, s.source_json, s.upstream, s.rules_json,
                    s.delivery_json, s.guest_json, s.enabled, s.created_at, s.updated_at
             FROM secrets s
             JOIN role_secrets rs ON rs.secret_id = s.id
             WHERE rs.role_id = ?1
             ORDER BY s.name, s.id",
        )?;
        collect_rows(statement.query_map(params![role_id], secret_row)?)
    }

    pub fn effective_secrets(&self, principal_id: &str) -> Result<Vec<SecretView>, PolicyError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT DISTINCT s.id, s.name, s.source_json, s.upstream, s.rules_json,
                    s.delivery_json, s.guest_json, s.enabled, s.created_at, s.updated_at
             FROM secrets s
             WHERE s.enabled = 1
               AND EXISTS (
                   SELECT 1 FROM principals p
                   WHERE p.id = ?1 AND p.enabled = 1
               )
               AND s.id IN (
                 SELECT secret_id FROM principal_secrets WHERE principal_id = ?1
                 UNION
                 SELECT rs.secret_id FROM role_secrets rs
                 JOIN principal_roles pr ON pr.role_id = rs.role_id
                 WHERE pr.principal_id = ?1
             )
             ORDER BY s.name, s.id",
        )?;
        collect_rows(statement.query_map(params![principal_id], secret_row)?)
    }

    pub fn effective_secret(
        &self,
        principal_id: &str,
        secret_id: &str,
    ) -> Result<SecretView, PolicyError> {
        self.effective_secrets(principal_id)?
            .into_iter()
            .find(|secret| secret.id == secret_id)
            .ok_or(PolicyError::Forbidden)
    }

    pub fn agent_effective_secret(
        &self,
        agent_id: &str,
        principal_id: &str,
        secret_id: &str,
    ) -> Result<SecretView, PolicyError> {
        let connection = self.lock()?;
        connection
            .query_row(
                "SELECT DISTINCT s.id, s.name, s.source_json, s.upstream, s.rules_json,
                        s.delivery_json, s.guest_json, s.enabled, s.created_at, s.updated_at
                 FROM secrets s
                 JOIN agents a ON a.id = ?1 AND a.principal_id = ?2
                 JOIN api_clients c ON c.id = a.owner_client_id AND c.enabled = 1
                 JOIN principals p ON p.id = a.principal_id AND p.enabled = 1
                 WHERE s.id = ?3 AND s.enabled = 1 AND s.id IN (
                     SELECT secret_id FROM principal_secrets WHERE principal_id = ?2
                     UNION
                     SELECT rs.secret_id FROM role_secrets rs
                     JOIN principal_roles pr ON pr.role_id = rs.role_id
                     WHERE pr.principal_id = ?2
                 )",
                params![agent_id, principal_id, secret_id],
                secret_row,
            )
            .optional()?
            .ok_or(PolicyError::Forbidden)
    }

    pub fn agent_effective_secrets(
        &self,
        agent_id: &str,
        principal_id: &str,
    ) -> Result<Vec<SecretView>, PolicyError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT DISTINCT s.id, s.name, s.source_json, s.upstream, s.rules_json,
                    s.delivery_json, s.guest_json, s.enabled, s.created_at, s.updated_at
             FROM secrets s
             JOIN agents a ON a.id = ?1 AND a.principal_id = ?2
             JOIN api_clients c ON c.id = a.owner_client_id AND c.enabled = 1
             JOIN principals p ON p.id = a.principal_id AND p.enabled = 1
             WHERE s.enabled = 1 AND s.id IN (
                 SELECT secret_id FROM principal_secrets WHERE principal_id = ?2
                 UNION
                 SELECT rs.secret_id FROM role_secrets rs
                 JOIN principal_roles pr ON pr.role_id = rs.role_id
                 WHERE pr.principal_id = ?2
             )
             ORDER BY s.name, s.id",
        )?;
        collect_rows(statement.query_map(params![agent_id, principal_id], secret_row)?)
    }

    pub fn secret_revision(&self) -> Result<u64, PolicyError> {
        let connection = self.lock()?;
        secret_revision(&connection)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, PolicyError> {
        self.connection.lock().map_err(|_| PolicyError::Poisoned)
    }
}

pub fn require(principal: &EffectivePrincipal, permission: &str) -> Result<(), PolicyError> {
    if principal.allows(permission) {
        Ok(())
    } else {
        Err(PolicyError::Forbidden)
    }
}

fn standard_agent_permissions() -> Vec<CapabilityName> {
    [
        "agent.new",
        "agent.read",
        "agent.turn",
        "agent.cancel",
        "agent.fork",
        "agent.evict",
        "agent.delete",
    ]
    .into_iter()
    .map(|name| CapabilityName::new(name).expect("static permission is valid"))
    .collect()
}

fn resolve_principal_id(
    connection: &Transaction<'_>,
    client: &AuthenticatedClient,
    context_key: Option<&str>,
) -> Result<String, PolicyError> {
    let bound = context_key
        .map(|context_key| {
            connection
                .query_row(
                    "SELECT principal_id FROM context_bindings
                     WHERE api_client_id = ?1 AND context_key = ?2",
                    params![client.id, context_key],
                    |row| row.get::<_, String>(0),
                )
                .optional()
        })
        .transpose()?
        .flatten();
    let principal_id = bound.unwrap_or_else(|| client.default_principal_id.clone());
    let enabled = connection
        .query_row(
            "SELECT enabled FROM principals WHERE id = ?1",
            params![principal_id],
            |row| row.get::<_, bool>(0),
        )
        .optional()?
        .ok_or(PolicyError::NotFound)?;
    if !enabled {
        return Err(PolicyError::Forbidden);
    }
    Ok(principal_id)
}

fn agent_identity(
    connection: &Connection,
    owner_client_id: &str,
    agent_id: &str,
) -> Result<AgentIdentity, PolicyError> {
    let record = connection
        .query_row(
            "SELECT id, owner_client_id, context_key, principal_id,
                    agent_config_json, created_at
             FROM agents WHERE id = ?1 AND owner_client_id = ?2",
            params![agent_id, owner_client_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )
        .optional()?
        .ok_or(PolicyError::NotFound)?;
    let enabled = connection
        .query_row(
            "SELECT enabled FROM principals WHERE id = ?1",
            params![record.3],
            |row| row.get::<_, bool>(0),
        )
        .optional()?
        .ok_or(PolicyError::NotFound)?;
    if !enabled {
        return Err(PolicyError::Forbidden);
    }
    Ok(AgentIdentity {
        id: record.0,
        owner_client_id: record.1,
        context_key: record.2,
        principal: EffectivePrincipal {
            id: record.3.clone(),
            agent_config: serde_json::from_str(&record.4)?,
            permissions: effective_capabilities(connection, &record.3)?,
            secret_revision: secret_revision(connection)?,
        },
        created_at: parse_time(&record.5)?,
    })
}

fn principal_config(
    connection: &Connection,
    principal_id: &str,
) -> Result<AgentConfig, PolicyError> {
    let encoded = connection
        .query_row(
            "SELECT agent_config_json FROM principals WHERE id = ?1 AND enabled = 1",
            params![principal_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or(PolicyError::NotFound)?;
    Ok(serde_json::from_str(&encoded)?)
}

fn effective_capabilities(
    connection: &Connection,
    principal_id: &str,
) -> Result<AgentCapabilities, PolicyError> {
    let mut statement = connection.prepare(
        "SELECT DISTINCT p.name FROM permissions p
         WHERE p.id IN (
             SELECT permission_id FROM principal_permissions WHERE principal_id = ?1
             UNION
             SELECT rp.permission_id FROM role_permissions rp
             JOIN principal_roles pr ON pr.role_id = rp.role_id
             WHERE pr.principal_id = ?1
         ) ORDER BY p.name",
    )?;
    let names = statement
        .query_map(params![principal_id], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let capabilities = names
        .into_iter()
        .map(|name| {
            CapabilityName::new(name)
                .map_err(|_| PolicyError::Invalid("invalid permission stored in database"))
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    Ok(AgentCapabilities::new(capabilities))
}

fn grant_direct(
    transaction: &Transaction<'_>,
    principal_id: &str,
    permission_name: &str,
) -> Result<(), PolicyError> {
    let permission_id = permission_name;
    transaction.execute(
        "INSERT OR IGNORE INTO permissions (id, name) VALUES (?1, ?1)",
        params![permission_id],
    )?;
    transaction.execute(
        "INSERT OR IGNORE INTO principal_permissions (principal_id, permission_id)
         VALUES (?1, ?2)",
        params![principal_id, permission_id],
    )?;
    Ok(())
}

fn set_join(
    connection: &Connection,
    table: &'static str,
    left_column: &'static str,
    left: &str,
    right_column: &'static str,
    right: &str,
    present: bool,
) -> Result<(), PolicyError> {
    let sql = if present {
        format!("INSERT OR IGNORE INTO {table} ({left_column}, {right_column}) VALUES (?1, ?2)")
    } else {
        format!("DELETE FROM {table} WHERE {left_column} = ?1 AND {right_column} = ?2")
    };
    let changed = connection.execute(&sql, params![left, right])?;
    if present && changed == 0 {
        // The relation already exists, which is a successful idempotent PUT.
        return Ok(());
    }
    if !present && changed == 0 {
        return Err(PolicyError::NotFound);
    }
    Ok(())
}

fn audit(
    transaction: &Transaction<'_>,
    actor: &str,
    action: &str,
    target_type: &str,
    target_id: &str,
) -> Result<(), PolicyError> {
    transaction.execute(
        "INSERT INTO audit_log
         (actor, action, target_type, target_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![actor, action, target_type, target_id, now()],
    )?;
    Ok(())
}

fn query_api_client(connection: &Connection, id: &str) -> Result<ApiClientView, PolicyError> {
    connection
        .query_row(
            "SELECT id, name, default_principal_id, enabled, created_at
             FROM api_clients WHERE id = ?1",
            params![id],
            api_client_row,
        )
        .optional()?
        .ok_or(PolicyError::NotFound)
}

fn query_principal(connection: &Connection, id: &str) -> Result<PrincipalView, PolicyError> {
    connection
        .query_row(
            "SELECT id, name, external_id, enabled, metadata_json,
                    agent_config_json, created_at
             FROM principals WHERE id = ?1",
            params![id],
            principal_row,
        )
        .optional()?
        .ok_or(PolicyError::NotFound)
}

fn api_client_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApiClientView> {
    let created_at = row.get::<_, String>(4)?;
    Ok(ApiClientView {
        id: row.get(0)?,
        name: row.get(1)?,
        default_principal_id: row.get(2)?,
        enabled: row.get(3)?,
        created_at: sqlite_time(&created_at)?,
    })
}

fn principal_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PrincipalView> {
    let metadata = row.get::<_, String>(4)?;
    let agent_config = row.get::<_, String>(5)?;
    let created_at = row.get::<_, String>(6)?;
    Ok(PrincipalView {
        id: row.get(0)?,
        name: row.get(1)?,
        external_id: row.get(2)?,
        enabled: row.get(3)?,
        metadata: serde_json::from_str(&metadata).map_err(json_sql_error)?,
        agent_config: serde_json::from_str(&agent_config).map_err(json_sql_error)?,
        created_at: sqlite_time(&created_at)?,
    })
}

fn context_binding_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ContextBindingView> {
    let created_at = row.get::<_, String>(4)?;
    Ok(ContextBindingView {
        id: row.get(0)?,
        api_client_id: row.get(1)?,
        context_key: row.get(2)?,
        principal_id: row.get(3)?,
        created_at: sqlite_time(&created_at)?,
    })
}

fn role_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RoleView> {
    Ok(RoleView {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
    })
}

fn permission_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PermissionView> {
    Ok(PermissionView {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
    })
}

fn secret_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SecretView> {
    let source = row.get::<_, String>(2)?;
    let rules = row.get::<_, String>(4)?;
    let delivery = row.get::<_, String>(5)?;
    let guest = row.get::<_, String>(6)?;
    let created_at = row.get::<_, String>(8)?;
    let updated_at = row.get::<_, String>(9)?;
    Ok(SecretView {
        id: row.get(0)?,
        name: row.get(1)?,
        source: serde_json::from_str::<SecretRef>(&source).map_err(json_sql_error)?,
        upstream: row.get(3)?,
        rules: serde_json::from_str::<Vec<SecretRequestRule>>(&rules).map_err(json_sql_error)?,
        delivery: serde_json::from_str::<SecretDelivery>(&delivery).map_err(json_sql_error)?,
        guest: serde_json::from_str::<SecretGuestConfig>(&guest).map_err(json_sql_error)?,
        enabled: row.get(7)?,
        created_at: sqlite_time(&created_at)?,
        updated_at: sqlite_time(&updated_at)?,
    })
}

fn query_secret(connection: &Connection, id: &str) -> Result<SecretView, PolicyError> {
    connection
        .query_row(
            "SELECT id, name, source_json, upstream, rules_json, delivery_json,
                    guest_json, enabled, created_at, updated_at
             FROM secrets WHERE id = ?1",
            params![id],
            secret_row,
        )
        .optional()?
        .ok_or(PolicyError::NotFound)
}

fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> Result<Vec<T>, PolicyError> {
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn sqlite_time(value: &str) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
}

fn json_sql_error(error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

fn parse_time(value: &str) -> Result<DateTime<Utc>, PolicyError> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| PolicyError::Invalid("invalid timestamp stored in database"))
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

fn hash_key(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

fn validate_context_key(value: Option<&str>) -> Result<(), PolicyError> {
    if value.is_some_and(|value| {
        value.is_empty()
            || value.len() > MAX_CONTEXT_KEY_BYTES
            || value.chars().any(char::is_control)
    }) {
        Err(PolicyError::Invalid(
            "context_key must contain 1 to 512 non-control bytes",
        ))
    } else {
        Ok(())
    }
}

fn validate_nonempty(value: &str, name: &'static str) -> Result<(), PolicyError> {
    if value.trim().is_empty() {
        Err(PolicyError::Invalid(name))
    } else {
        Ok(())
    }
}

fn validate_agent_config(config: &AgentConfig) -> Result<(), PolicyError> {
    if config.instructions.as_ref().is_some_and(|instructions| {
        instructions.trim().is_empty() || instructions.len() > MAX_INSTRUCTIONS_BYTES
    }) {
        Err(PolicyError::Invalid(
            "instructions must contain 1 to 65536 bytes",
        ))
    } else {
        Ok(())
    }
}

fn changed(count: usize) -> Result<(), PolicyError> {
    if count == 0 {
        Err(PolicyError::NotFound)
    } else {
        Ok(())
    }
}

fn secret_revision(connection: &Connection) -> Result<u64, PolicyError> {
    let value = connection.query_row(
        "SELECT value FROM policy_state WHERE key = 'secret_revision'",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    u64::try_from(value).map_err(|_| PolicyError::Invalid("invalid secret revision"))
}

fn bump_secret_revision(connection: &Connection) -> Result<(), PolicyError> {
    connection.execute(
        "UPDATE policy_state SET value = value + 1 WHERE key = 'secret_revision'",
        [],
    )?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("authentication required")]
    Unauthenticated,
    #[error("permission denied")]
    Forbidden,
    #[error("policy object was not found")]
    NotFound,
    #[error("invalid policy input: {0}")]
    Invalid(&'static str),
    #[error("policy database lock was poisoned")]
    Poisoned,
    #[error("policy database failed")]
    Database(#[from] rusqlite::Error),
    #[error("policy JSON failed")]
    Json(#[from] serde_json::Error),
    #[error("policy filesystem failed")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn context_resolution_is_scoped_and_agent_config_is_snapshotted() {
        let store = PolicyStore::in_memory().unwrap();
        store
            .bootstrap("client", "Client", "key", "default", [])
            .unwrap();
        let channel = store
            .create_principal(CreatePrincipal {
                id: Some("channel".to_owned()),
                name: "Channel".to_owned(),
                external_id: None,
                metadata: PrincipalMetadata::default(),
                agent_config: AgentConfig {
                    instructions: Some("channel instructions".to_owned()),
                    ..AgentConfig::default()
                },
            })
            .unwrap();
        store
            .create_context_binding(CreateContextBinding {
                api_client_id: "client".to_owned(),
                context_key: "channel:1".to_owned(),
                principal_id: channel.id,
            })
            .unwrap();
        store
            .set_principal_permission("channel", "agent.new", true)
            .unwrap();
        store
            .set_principal_permission("channel", "agent.read", true)
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("key"));
        let client = store.authenticate(&headers).unwrap();
        let (first, created) = store
            .create_or_resolve_agent(&client, Some("channel:1"))
            .unwrap();
        assert!(created);
        assert_eq!(first.principal.id, "channel");
        assert_eq!(
            first.principal.agent_config.instructions.as_deref(),
            Some("channel instructions")
        );
        let (same, created) = store
            .create_or_resolve_agent(&client, Some("channel:1"))
            .unwrap();
        assert!(!created);
        assert_eq!(same.id, first.id);
    }

    #[test]
    fn roles_and_direct_grants_union_into_effective_permissions() {
        let store = PolicyStore::in_memory().unwrap();
        store
            .bootstrap("client", "Client", "key", "principal", [])
            .unwrap();
        let role = store
            .create_role(CreateRole {
                id: Some("reader".to_owned()),
                name: "reader".to_owned(),
                description: None,
            })
            .unwrap();
        let permission = store
            .create_permission(CreatePermission {
                id: Some("github.read".to_owned()),
                name: "github.read".to_owned(),
                description: None,
            })
            .unwrap();
        store
            .set_principal_role("principal", &role.id, true)
            .unwrap();
        store
            .set_role_permission(&role.id, &permission.id, true)
            .unwrap();
        assert!(
            store
                .effective_permissions("principal")
                .unwrap()
                .iter()
                .any(|permission| permission.name == "github.read")
        );
    }

    fn test_secret(id: &str) -> CreateSecret {
        CreateSecret {
            id: Some(id.to_owned()),
            name: id.to_owned(),
            source: SecretRef {
                provider: "environment".to_owned(),
                key: "OPENAI".to_owned(),
            },
            upstream: "https://api.openai.com".to_owned(),
            rules: vec![SecretRequestRule {
                methods: BTreeSet::from([crate::SecretHttpMethod::Get]),
                path_prefixes: vec!["/v1/".to_owned()],
            }],
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
    fn direct_and_role_secret_grants_survive_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("policy.sqlite");
        let store = PolicyStore::open(&path).unwrap();
        store
            .bootstrap("client", "Client", "key", "principal", [])
            .unwrap();
        store.create_secret(test_secret("direct")).unwrap();
        store.create_secret(test_secret("role-secret")).unwrap();
        let role = store
            .create_role(CreateRole {
                id: Some("role".to_owned()),
                name: "role".to_owned(),
                description: None,
            })
            .unwrap();
        let before = store.secret_revision().unwrap();
        store
            .set_principal_secret("principal", "direct", true)
            .unwrap();
        store
            .set_role_secret(&role.id, "role-secret", true)
            .unwrap();
        store
            .set_principal_role("principal", &role.id, true)
            .unwrap();

        let effective = store.effective_secrets("principal").unwrap();
        assert_eq!(
            effective
                .iter()
                .map(|secret| secret.id.as_str())
                .collect::<Vec<_>>(),
            vec!["direct", "role-secret"]
        );
        assert!(store.secret_revision().unwrap() > before);

        drop(store);
        let store = PolicyStore::open(&path).unwrap();
        assert_eq!(store.effective_secrets("principal").unwrap().len(), 2);
        store
            .set_principal_secret("principal", "direct", false)
            .unwrap();
        assert!(matches!(
            store.effective_secret("principal", "direct"),
            Err(PolicyError::Forbidden)
        ));
    }
}

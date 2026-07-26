use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, post, put},
};

use crate::{
    ApiState, CreateApiClient, CreateApiKey, CreateContextBinding, CreatePermission,
    CreatePrincipal, CreateRole, CreateSecret, PatchApiClient, PatchContextBinding, PatchPrincipal,
    PatchRole, PatchSecret, ResolveContext, api::ApiError,
};

pub(crate) fn routes() -> Router<Arc<ApiState>> {
    Router::new()
        .route(
            "/admin/v1/api-clients",
            post(create_api_client).get(list_api_clients),
        )
        .route(
            "/admin/v1/api-clients/{id}",
            get(get_api_client)
                .patch(patch_api_client)
                .delete(delete_api_client),
        )
        .route("/admin/v1/api-clients/{id}/keys", post(add_api_key))
        .route(
            "/admin/v1/api-clients/{id}/keys/{key_id}",
            delete(delete_api_key),
        )
        .route(
            "/admin/v1/principals",
            post(create_principal).get(list_principals),
        )
        .route(
            "/admin/v1/principals/{id}",
            get(get_principal)
                .patch(patch_principal)
                .delete(delete_principal),
        )
        .route(
            "/admin/v1/context-bindings",
            post(create_context_binding).get(list_context_bindings),
        )
        .route(
            "/admin/v1/context-bindings/{id}",
            get(get_context_binding)
                .patch(patch_context_binding)
                .delete(delete_context_binding),
        )
        .route("/admin/v1/context-bindings/resolve", post(resolve_context))
        .route("/admin/v1/roles", post(create_role).get(list_roles))
        .route(
            "/admin/v1/roles/{id}",
            get(get_role).patch(patch_role).delete(delete_role),
        )
        .route(
            "/admin/v1/permissions",
            post(create_permission).get(list_permissions),
        )
        .route(
            "/admin/v1/permissions/{id}",
            get(get_permission).delete(delete_permission),
        )
        .route(
            "/admin/v1/principals/{principal_id}/roles",
            get(list_principal_roles),
        )
        .route(
            "/admin/v1/principals/{principal_id}/roles/{role_id}",
            put(add_principal_role).delete(remove_principal_role),
        )
        .route(
            "/admin/v1/roles/{role_id}/permissions",
            get(list_role_permissions),
        )
        .route(
            "/admin/v1/roles/{role_id}/permissions/{permission_id}",
            put(add_role_permission).delete(remove_role_permission),
        )
        .route(
            "/admin/v1/principals/{principal_id}/permissions",
            get(list_principal_permissions),
        )
        .route(
            "/admin/v1/principals/{principal_id}/permissions/{permission_id}",
            put(add_principal_permission).delete(remove_principal_permission),
        )
        .route(
            "/admin/v1/principals/{principal_id}/effective-permissions",
            get(effective_permissions),
        )
        .route("/admin/v1/secrets", post(create_secret).get(list_secrets))
        .route(
            "/admin/v1/secrets/{id}",
            get(get_secret).patch(patch_secret).delete(delete_secret),
        )
        .route(
            "/admin/v1/principals/{principal_id}/secrets",
            get(list_principal_secrets),
        )
        .route(
            "/admin/v1/principals/{principal_id}/secrets/{secret_id}",
            put(add_principal_secret).delete(remove_principal_secret),
        )
        .route("/admin/v1/roles/{role_id}/secrets", get(list_role_secrets))
        .route(
            "/admin/v1/roles/{role_id}/secrets/{secret_id}",
            put(add_role_secret).delete(remove_role_secret),
        )
        .route(
            "/admin/v1/principals/{principal_id}/effective-secrets",
            get(effective_secrets),
        )
}

fn authorize(state: &ApiState, headers: &HeaderMap) -> Result<(), ApiError> {
    state.admin.authorize(headers)?;
    Ok(())
}

async fn create_api_client(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Json(request): Json<CreateApiClient>,
) -> Result<(StatusCode, Json<crate::ApiClientView>), ApiError> {
    authorize(&state, &headers)?;
    Ok((
        StatusCode::CREATED,
        Json(state.policy.create_api_client(request)?),
    ))
}

async fn list_api_clients(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<crate::ApiClientView>>, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.policy.api_clients()?))
}

async fn get_api_client(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<crate::ApiClientView>, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.policy.api_client(&id)?))
}

async fn patch_api_client(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(patch): Json<PatchApiClient>,
) -> Result<Json<crate::ApiClientView>, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.policy.patch_api_client(&id, patch)?))
}

async fn delete_api_client(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    authorize(&state, &headers)?;
    state.policy.disable_api_client(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn add_api_key(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(request): Json<CreateApiKey>,
) -> Result<(StatusCode, Json<crate::ApiKeyView>), ApiError> {
    authorize(&state, &headers)?;
    Ok((
        StatusCode::CREATED,
        Json(state.policy.add_api_key(&id, request)?),
    ))
}

async fn delete_api_key(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path((id, key_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    authorize(&state, &headers)?;
    state.policy.delete_api_key(&id, &key_id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create_principal(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Json(request): Json<CreatePrincipal>,
) -> Result<(StatusCode, Json<crate::PrincipalView>), ApiError> {
    authorize(&state, &headers)?;
    Ok((
        StatusCode::CREATED,
        Json(state.policy.create_principal(request)?),
    ))
}

async fn list_principals(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<crate::PrincipalView>>, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.policy.principals()?))
}

async fn get_principal(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<crate::PrincipalView>, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.policy.principal(&id)?))
}

async fn patch_principal(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(patch): Json<PatchPrincipal>,
) -> Result<Json<crate::PrincipalView>, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.policy.patch_principal(&id, patch)?))
}

async fn delete_principal(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    authorize(&state, &headers)?;
    state.policy.disable_principal(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create_context_binding(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Json(request): Json<CreateContextBinding>,
) -> Result<(StatusCode, Json<crate::ContextBindingView>), ApiError> {
    authorize(&state, &headers)?;
    Ok((
        StatusCode::CREATED,
        Json(state.policy.create_context_binding(request)?),
    ))
}

async fn list_context_bindings(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<crate::ContextBindingView>>, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.policy.context_bindings()?))
}

async fn get_context_binding(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<crate::ContextBindingView>, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.policy.context_binding(&id)?))
}

async fn patch_context_binding(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(patch): Json<PatchContextBinding>,
) -> Result<Json<crate::ContextBindingView>, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.policy.patch_context_binding(&id, patch)?))
}

async fn delete_context_binding(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    authorize(&state, &headers)?;
    state.policy.delete_context_binding(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn resolve_context(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Json(request): Json<ResolveContext>,
) -> Result<Json<crate::ResolvedContextView>, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.policy.resolve_context(request)?))
}

async fn create_role(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Json(request): Json<CreateRole>,
) -> Result<(StatusCode, Json<crate::RoleView>), ApiError> {
    authorize(&state, &headers)?;
    Ok((
        StatusCode::CREATED,
        Json(state.policy.create_role(request)?),
    ))
}

async fn list_roles(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<crate::RoleView>>, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.policy.roles()?))
}

async fn get_role(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<crate::RoleView>, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.policy.role(&id)?))
}

async fn patch_role(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(patch): Json<PatchRole>,
) -> Result<Json<crate::RoleView>, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.policy.patch_role(&id, patch)?))
}

async fn delete_role(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    authorize(&state, &headers)?;
    state.policy.delete_role(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create_permission(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Json(request): Json<CreatePermission>,
) -> Result<(StatusCode, Json<crate::PermissionView>), ApiError> {
    authorize(&state, &headers)?;
    Ok((
        StatusCode::CREATED,
        Json(state.policy.create_permission(request)?),
    ))
}

async fn list_permissions(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<crate::PermissionView>>, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.policy.permissions()?))
}

async fn get_permission(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<crate::PermissionView>, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.policy.permission(&id)?))
}

async fn delete_permission(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    authorize(&state, &headers)?;
    state.policy.delete_permission(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn add_principal_role(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path((principal_id, role_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    authorize(&state, &headers)?;
    state
        .policy
        .set_principal_role(&principal_id, &role_id, true)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_principal_role(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path((principal_id, role_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    authorize(&state, &headers)?;
    state
        .policy
        .set_principal_role(&principal_id, &role_id, false)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_principal_roles(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(principal_id): Path<String>,
) -> Result<Json<Vec<crate::RoleView>>, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.policy.principal_roles(&principal_id)?))
}

async fn add_role_permission(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path((role_id, permission_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    authorize(&state, &headers)?;
    state
        .policy
        .set_role_permission(&role_id, &permission_id, true)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_role_permission(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path((role_id, permission_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    authorize(&state, &headers)?;
    state
        .policy
        .set_role_permission(&role_id, &permission_id, false)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_role_permissions(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(role_id): Path<String>,
) -> Result<Json<Vec<crate::PermissionView>>, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.policy.role_permissions(&role_id)?))
}

async fn add_principal_permission(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path((principal_id, permission_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    authorize(&state, &headers)?;
    state
        .policy
        .set_principal_permission(&principal_id, &permission_id, true)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_principal_permission(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path((principal_id, permission_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    authorize(&state, &headers)?;
    state
        .policy
        .set_principal_permission(&principal_id, &permission_id, false)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_principal_permissions(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(principal_id): Path<String>,
) -> Result<Json<Vec<crate::PermissionView>>, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.policy.principal_permissions(&principal_id)?))
}

async fn effective_permissions(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(principal_id): Path<String>,
) -> Result<Json<Vec<crate::PermissionView>>, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.policy.effective_permissions(&principal_id)?))
}

async fn create_secret(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Json(request): Json<CreateSecret>,
) -> Result<(StatusCode, Json<crate::SecretView>), ApiError> {
    authorize(&state, &headers)?;
    Ok((
        StatusCode::CREATED,
        Json(state.policy.create_secret(request)?),
    ))
}

async fn list_secrets(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<crate::SecretView>>, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.policy.secrets()?))
}

async fn get_secret(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<crate::SecretView>, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.policy.secret(&id)?))
}

async fn patch_secret(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(patch): Json<PatchSecret>,
) -> Result<Json<crate::SecretView>, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.policy.patch_secret(&id, patch)?))
}

async fn delete_secret(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    authorize(&state, &headers)?;
    state.policy.delete_secret(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_principal_secrets(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(principal_id): Path<String>,
) -> Result<Json<Vec<crate::SecretView>>, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.policy.principal_secrets(&principal_id)?))
}

async fn add_principal_secret(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path((principal_id, secret_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    authorize(&state, &headers)?;
    state
        .policy
        .set_principal_secret(&principal_id, &secret_id, true)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_principal_secret(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path((principal_id, secret_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    authorize(&state, &headers)?;
    state
        .policy
        .set_principal_secret(&principal_id, &secret_id, false)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_role_secrets(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(role_id): Path<String>,
) -> Result<Json<Vec<crate::SecretView>>, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.policy.role_secrets(&role_id)?))
}

async fn add_role_secret(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path((role_id, secret_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    authorize(&state, &headers)?;
    state.policy.set_role_secret(&role_id, &secret_id, true)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_role_secret(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path((role_id, secret_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    authorize(&state, &headers)?;
    state.policy.set_role_secret(&role_id, &secret_id, false)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn effective_secrets(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(principal_id): Path<String>,
) -> Result<Json<Vec<crate::SecretView>>, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.policy.effective_secrets(&principal_id)?))
}

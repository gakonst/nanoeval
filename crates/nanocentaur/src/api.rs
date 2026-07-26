use std::{convert::Infallible, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{any, get, post},
};
use futures_util::stream;
use serde::{Deserialize, Serialize};

use crate::{
    AdminAuthorizer, AgentManager, AuthorizationError, CreateAgent, CreateAgentResponse,
    CreateTurn, ManagerError, PaymentError, PaymentGate, PaymentOutcome, PolicyError, PolicyStore,
    SecretGateway, TurnAction, TurnView, require,
};

pub struct ApiState {
    pub manager: Arc<AgentManager>,
    pub policy: Arc<PolicyStore>,
    pub admin: Arc<AdminAuthorizer>,
    pub payments: Arc<dyn PaymentGate>,
    pub secrets: Arc<SecretGateway>,
}

pub fn app(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/agent/new", post(create_agent))
        .route("/v1/agent/{agent_id}", get(get_agent).delete(delete_agent))
        .route("/v1/agent/{agent_id}/turn", post(create_turn))
        .route("/v1/agent/{agent_id}/turn/{turn_id}", get(get_turn))
        .route(
            "/v1/agent/{agent_id}/turn/{turn_id}/cancel",
            post(cancel_turn),
        )
        .route("/v1/agent/{agent_id}/events", get(agent_events))
        .route("/v1/agent/{agent_id}/fork", post(fork_latest))
        .route(
            "/v1/agent/{agent_id}/turn/{turn_id}/fork",
            post(fork_from_turn),
        )
        .route("/v1/agent/{agent_id}/evict", post(evict_agent))
        .route("/v1/payment-sessions", post(payment_session))
        .route(
            "/internal/v1/secret-egress/{lease_token}/{secret_id}",
            any(secret_gateway_root),
        )
        .route(
            "/internal/v1/secret-egress/{lease_token}/{secret_id}/{*path}",
            any(secret_gateway_path),
        )
        .merge(crate::admin::routes())
        .with_state(state)
}

#[derive(Serialize)]
struct HealthResponse {
    status: HealthStatus,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum HealthStatus {
    Ok,
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: HealthStatus::Ok,
    })
}

async fn secret_gateway_root(
    State(state): State<Arc<ApiState>>,
    Path((lease_token, secret_id)): Path<(String, String)>,
    request: Request,
) -> Response {
    state
        .secrets
        .handle(&lease_token, &secret_id, "", request)
        .await
}

async fn secret_gateway_path(
    State(state): State<Arc<ApiState>>,
    Path((lease_token, secret_id, path)): Path<(String, String, String)>,
    request: Request,
) -> Response {
    state
        .secrets
        .handle(&lease_token, &secret_id, &path, request)
        .await
}

async fn create_agent(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Json(request): Json<CreateAgent>,
) -> Result<Response, ApiError> {
    let client = state.policy.authenticate(&headers)?;
    let (identity, created) = state
        .policy
        .create_or_resolve_agent(&client, request.context_key.as_deref())?;
    let view = state.manager.register(identity).await?;
    let response = CreateAgentResponse {
        agent_id: view.agent_id,
        created,
        state: view.state,
    };
    Ok((
        if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(response),
    )
        .into_response())
}

async fn get_agent(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
) -> Result<Json<crate::AgentView>, ApiError> {
    let (identity, _) = authorize_agent(&state, &headers, &agent_id, "agent.read")?;
    Ok(Json(state.manager.get(identity).await?))
}

async fn delete_agent(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let (identity, client) = authorize_agent(&state, &headers, &agent_id, "agent.delete")?;
    if state.manager.get(identity).await?.state == crate::AgentStatus::Running {
        return Err(ManagerError::AgentBusy.into());
    }
    state.manager.delete(&agent_id).await?;
    state.policy.delete_agent(&client, &agent_id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create_turn(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
    Json(request): Json<CreateTurn>,
) -> Result<Response, ApiError> {
    let (identity, _) = authorize_agent(&state, &headers, &agent_id, "agent.turn")?;
    let idempotency_key = idempotency_key(&headers)?;
    if let Some(key) = idempotency_key.as_deref()
        && let Some(response) = state
            .manager
            .find_turn_by_idempotency_key(identity.clone(), key)
            .await?
    {
        return Ok(Json(response).into_response());
    }

    let receipt = match state.payments.authorize(&headers).await? {
        PaymentOutcome::Authorized(receipt) => receipt,
        outcome => return payment_response(outcome),
    };
    let response = state
        .manager
        .create_turn(identity, request, idempotency_key)
        .await?;
    let status = match response.action {
        TurnAction::Steered => StatusCode::OK,
        TurnAction::Started | TurnAction::Queued => StatusCode::ACCEPTED,
    };
    let mut response = (status, Json(response)).into_response();
    insert_receipt(&mut response, &receipt.header_value)?;
    Ok(response)
}

async fn get_turn(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path((agent_id, turn_id)): Path<(String, String)>,
) -> Result<Json<TurnView>, ApiError> {
    let (identity, _) = authorize_agent(&state, &headers, &agent_id, "agent.read")?;
    Ok(Json(state.manager.get_turn(identity, &turn_id).await?))
}

#[derive(Default, Deserialize)]
struct EventsQuery {
    after_event_id: Option<u64>,
}

async fn agent_events(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> Result<Response, ApiError> {
    let (identity, _) = authorize_agent(&state, &headers, &agent_id, "agent.read")?;
    let after_event_id = last_event_id(&headers)?
        .or(query.after_event_id)
        .unwrap_or(0);
    let cursor = state.manager.events(identity, after_event_id).await?;
    let output = stream::unfold(Some(cursor), |cursor| async move {
        let mut cursor = cursor?;
        let event = cursor.next().await?;
        let sse = Event::default()
            .id(event.id.to_string())
            .event(event.payload.event_name())
            .json_data(&event)
            .unwrap_or_else(|_| Event::default().event("stream.error").data("{}"));
        Some((Ok::<Event, Infallible>(sse), Some(cursor)))
    });
    Ok(Sse::new(output)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keepalive"),
        )
        .into_response())
}

#[derive(Serialize)]
struct CancelTurnResponse {
    cancel_requested: bool,
}

async fn cancel_turn(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path((agent_id, turn_id)): Path<(String, String)>,
) -> Result<Json<CancelTurnResponse>, ApiError> {
    let (identity, _) = authorize_agent(&state, &headers, &agent_id, "agent.cancel")?;
    let cancelled = state.manager.cancel_turn(identity, &turn_id).await?;
    Ok(Json(CancelTurnResponse {
        cancel_requested: cancelled,
    }))
}

#[derive(Serialize)]
struct EvictAgentResponse {
    evicted: bool,
}

async fn evict_agent(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
) -> Result<Json<EvictAgentResponse>, ApiError> {
    let (identity, _) = authorize_agent(&state, &headers, &agent_id, "agent.evict")?;
    let evicted = state.manager.evict(identity).await?;
    Ok(Json(EvictAgentResponse { evicted }))
}

async fn fork_latest(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
) -> Result<Response, ApiError> {
    fork_agent(&state, &headers, &agent_id, None).await
}

async fn fork_from_turn(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path((agent_id, turn_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    fork_agent(&state, &headers, &agent_id, Some(&turn_id)).await
}

async fn fork_agent(
    state: &ApiState,
    headers: &HeaderMap,
    agent_id: &str,
    turn_id: Option<&str>,
) -> Result<Response, ApiError> {
    let (source, client) = authorize_agent(state, headers, agent_id, "agent.fork")?;
    let target = state.policy.fork_agent(&client, agent_id)?;
    match state.manager.fork(source, target.clone(), turn_id).await {
        Ok(response) => Ok((StatusCode::CREATED, Json(response)).into_response()),
        Err(error) => {
            if let Err(cleanup_error) = state.policy.delete_agent(&client, &target.id) {
                tracing::warn!(%cleanup_error, "failed to clean up fork registry record");
            }
            Err(error.into())
        }
    }
}

async fn payment_session(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let _ = state.policy.authenticate(&headers)?;
    payment_response(state.payments.authorize(&headers).await?)
}

fn authorize_agent(
    state: &ApiState,
    headers: &HeaderMap,
    agent_id: &str,
    permission: &str,
) -> Result<(crate::AgentIdentity, crate::AuthenticatedClient), ApiError> {
    let client = state.policy.authenticate(headers)?;
    let identity = state.policy.agent(&client, agent_id)?;
    require(&identity.principal, permission)?;
    Ok((identity, client))
}

fn payment_response(outcome: PaymentOutcome) -> Result<Response, ApiError> {
    match outcome {
        PaymentOutcome::Challenge { www_authenticate } => {
            let mut response = (
                StatusCode::PAYMENT_REQUIRED,
                Json(ErrorResponse::new(
                    "payment_required",
                    "payment authorization required",
                )),
            )
                .into_response();
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_str(&www_authenticate).map_err(|_| ApiError::Internal)?,
            );
            Ok(response)
        }
        PaymentOutcome::Management { body, receipt } => {
            let mut response = Json(body).into_response();
            insert_receipt(&mut response, &receipt.header_value)?;
            Ok(response)
        }
        PaymentOutcome::Authorized(receipt) => {
            let mut response = StatusCode::NO_CONTENT.into_response();
            insert_receipt(&mut response, &receipt.header_value)?;
            Ok(response)
        }
    }
}

fn insert_receipt(response: &mut Response, value: &str) -> Result<(), ApiError> {
    response.headers_mut().insert(
        HeaderName::from_static("payment-receipt"),
        HeaderValue::from_str(value).map_err(|_| ApiError::Internal)?,
    );
    Ok(())
}

fn idempotency_key(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    optional_header(headers, "idempotency-key", "Idempotency-Key")
}

fn last_event_id(headers: &HeaderMap) -> Result<Option<u64>, ApiError> {
    optional_header(headers, "last-event-id", "Last-Event-ID")?
        .map(|value| {
            value
                .parse()
                .map_err(|_| ApiError::BadHeader("Last-Event-ID"))
        })
        .transpose()
}

fn optional_header(
    headers: &HeaderMap,
    header_name: &'static str,
    display_name: &'static str,
) -> Result<Option<String>, ApiError> {
    headers
        .get(header_name)
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .map_err(|_| ApiError::BadHeader(display_name))
        })
        .transpose()
}

pub(crate) enum ApiError {
    Authorization(AuthorizationError),
    Policy(PolicyError),
    Manager(ManagerError),
    Payment(PaymentError),
    BadHeader(&'static str),
    Internal,
}

impl From<AuthorizationError> for ApiError {
    fn from(error: AuthorizationError) -> Self {
        Self::Authorization(error)
    }
}

impl From<PolicyError> for ApiError {
    fn from(error: PolicyError) -> Self {
        Self::Policy(error)
    }
}

impl From<ManagerError> for ApiError {
    fn from(error: ManagerError) -> Self {
        Self::Manager(error)
    }
}

impl From<PaymentError> for ApiError {
    fn from(error: PaymentError) -> Self {
        Self::Payment(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            Self::Authorization(AuthorizationError::Unauthenticated)
            | Self::Policy(PolicyError::Unauthenticated) => (
                StatusCode::UNAUTHORIZED,
                "unauthenticated",
                "authentication required",
            ),
            Self::Policy(PolicyError::Forbidden) => (
                StatusCode::FORBIDDEN,
                "permission_denied",
                "permission denied",
            ),
            Self::Policy(PolicyError::NotFound) | Self::Manager(ManagerError::NotFound) => {
                (StatusCode::NOT_FOUND, "not_found", "resource not found")
            }
            Self::Manager(ManagerError::SteerQueueFull) => (
                StatusCode::CONFLICT,
                "steer_queue_full",
                "active turn steering queue is full",
            ),
            Self::Manager(ManagerError::AgentBusy) => (
                StatusCode::CONFLICT,
                "agent_busy",
                "agent must be idle for this operation",
            ),
            Self::Manager(ManagerError::ForkBoundaryNotFound) => (
                StatusCode::CONFLICT,
                "fork_boundary_not_found",
                "completed fork boundary was not found",
            ),
            Self::Manager(ManagerError::Invalid(message))
            | Self::Policy(PolicyError::Invalid(message)) => {
                (StatusCode::BAD_REQUEST, "invalid_request", message)
            }
            Self::Payment(PaymentError::InvalidCredential) => (
                StatusCode::PAYMENT_REQUIRED,
                "invalid_payment",
                "invalid payment credential",
            ),
            Self::Payment(PaymentError::Configuration(_) | PaymentError::Verification(_))
            | Self::Policy(
                PolicyError::Poisoned
                | PolicyError::Database(_)
                | PolicyError::Json(_)
                | PolicyError::Io(_),
            )
            | Self::Manager(
                ManagerError::ActorStopped
                | ManagerError::Durability(_)
                | ManagerError::Agent(_)
                | ManagerError::Io(_),
            )
            | Self::Authorization(AuthorizationError::InvalidConfiguration(_))
            | Self::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "internal server error",
            ),
            Self::BadHeader(name) => (StatusCode::BAD_REQUEST, "invalid_header", name),
        };
        (status, Json(ErrorResponse::new(code, message))).into_response()
    }
}

#[derive(Serialize)]
struct ErrorResponse {
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    message: &'static str,
}

impl ErrorResponse {
    const fn new(code: &'static str, message: &'static str) -> Self {
        Self {
            error: ErrorBody { code, message },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use serde::de::DeserializeOwned;
    use tower::ServiceExt;

    use super::*;
    use crate::{
        AdminAuthorizer, CapabilityEgress, CompositeSecretManager, FreePaymentGate,
        ManagedAgentFactory, MockAgentFactory, PolicyStore, SecretGateway,
    };

    async fn response_json<T: DeserializeOwned>(response: Response) -> T {
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap()
    }

    fn test_app(delay: Duration) -> Router {
        let factory: Arc<dyn ManagedAgentFactory> = Arc::new(MockAgentFactory::new(delay));
        let directory = tempfile::tempdir().unwrap().keep();
        let policy = Arc::new(PolicyStore::in_memory().unwrap());
        policy
            .bootstrap("test", "Test", "test-key", "test", [])
            .unwrap();
        let secrets = Arc::new(
            SecretGateway::new(
                Arc::clone(&policy),
                Arc::new(CompositeSecretManager::new()),
                Arc::new(CapabilityEgress::new()),
                "http://127.0.0.1:3000",
            )
            .unwrap(),
        );
        let state = Arc::new(ApiState {
            manager: Arc::new(AgentManager::new(factory, directory).unwrap()),
            policy,
            admin: Arc::new(AdminAuthorizer::new("admin-key").unwrap()),
            payments: Arc::new(FreePaymentGate),
            secrets,
        });
        app(state)
    }

    async fn create_test_agent(app: &Router, context_key: &str) -> String {
        let response = app
            .clone()
            .oneshot(
                Request::post("/v1/agent/new")
                    .header("x-api-key", "test-key")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(r#"{{"context_key":"{context_key}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        response_json::<CreateAgentResponse>(response)
            .await
            .agent_id
    }

    async fn wait_for_test_turn(app: &Router, agent_id: &str, turn_id: &str) -> TurnView {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let response = app
                    .clone()
                    .oneshot(
                        Request::get(format!("/v1/agent/{agent_id}/turn/{turn_id}"))
                            .header("x-api-key", "test-key")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let turn = response_json::<TurnView>(response).await;
                if turn.state.is_terminal() {
                    return turn;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn context_key_creates_or_returns_the_same_agent() {
        let app = test_app(Duration::from_millis(5));
        let first = create_test_agent(&app, "context:1").await;
        let response = app
            .oneshot(
                Request::post("/v1/agent/new")
                    .header("x-api-key", "test-key")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"context_key":"context:1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json::<CreateAgentResponse>(response).await;
        assert_eq!(body.agent_id, first);
        assert!(!body.created);
    }

    #[tokio::test]
    async fn follow_on_messages_steer_unless_enqueue_is_explicit() {
        let app = test_app(Duration::from_millis(100));
        let agent_id = create_test_agent(&app, "context:steer").await;
        let send = |body: &'static str| {
            Request::post(format!("/v1/agent/{agent_id}/turn"))
                .header("x-api-key", "test-key")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap()
        };
        let started = app
            .clone()
            .oneshot(send(r#"{"content":[{"type":"text","text":"one"}]}"#))
            .await
            .unwrap();
        let started = response_json::<crate::TurnActionResponse>(started).await;
        assert_eq!(started.action, TurnAction::Started);
        tokio::time::sleep(Duration::from_millis(10)).await;

        let steered = app
            .clone()
            .oneshot(send(r#"{"content":[{"type":"text","text":"two"}]}"#))
            .await
            .unwrap();
        let steered = response_json::<crate::TurnActionResponse>(steered).await;
        assert_eq!(steered.action, TurnAction::Steered);
        assert_eq!(steered.turn_id, started.turn_id);

        let queued = app
            .oneshot(send(
                r#"{"delivery":"enqueue","content":[{"type":"text","text":"three"}]}"#,
            ))
            .await
            .unwrap();
        let queued = response_json::<crate::TurnActionResponse>(queued).await;
        assert_eq!(queued.action, TurnAction::Queued);
        assert_ne!(queued.turn_id, started.turn_id);
    }

    #[tokio::test]
    async fn nested_turn_result_route_returns_content_blocks() {
        let app = test_app(Duration::from_millis(5));
        let agent_id = create_test_agent(&app, "context:result").await;
        let created = app
            .clone()
            .oneshot(
                Request::post(format!("/v1/agent/{agent_id}/turn"))
                    .header("x-api-key", "test-key")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"content":[{"type":"text","text":"hello"}]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let turn_id = response_json::<crate::TurnActionResponse>(created)
            .await
            .turn_id;
        wait_for_test_turn(&app, &agent_id, &turn_id).await;
        let result = app
            .oneshot(
                Request::get(format!("/v1/agent/{agent_id}/turn/{turn_id}"))
                    .header("x-api-key", "test-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let result = response_json::<TurnView>(result).await;
        assert_eq!(result.state, crate::TurnStatus::Completed);
        assert!(matches!(
            result.output.first(),
            Some(crate::ContentBlock::Text { .. })
        ));
    }

    #[tokio::test]
    async fn completed_agent_can_fork_through_the_nested_route() {
        let app = test_app(Duration::from_millis(5));
        let agent_id = create_test_agent(&app, "context:fork").await;
        let created = app
            .clone()
            .oneshot(
                Request::post(format!("/v1/agent/{agent_id}/turn"))
                    .header("x-api-key", "test-key")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"content":[{"type":"text","text":"establish a boundary"}]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let turn_id = response_json::<crate::TurnActionResponse>(created)
            .await
            .turn_id;
        wait_for_test_turn(&app, &agent_id, &turn_id).await;

        let forked = app
            .oneshot(
                Request::post(format!("/v1/agent/{agent_id}/turn/{turn_id}/fork"))
                    .header("x-api-key", "test-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(forked.status(), StatusCode::CREATED);
        let forked = response_json::<crate::ForkResponse>(forked).await;
        assert_ne!(forked.agent_id, agent_id);
        assert_eq!(
            forked.forked_from.turn_id.as_deref(),
            Some(turn_id.as_str())
        );
    }

    #[tokio::test]
    async fn admin_routes_reject_agent_credentials() {
        let app = test_app(Duration::from_millis(5));
        let response = app
            .oneshot(
                Request::get("/admin/v1/principals")
                    .header("x-api-key", "test-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn admin_can_create_and_grant_a_typed_secret() {
        let app = test_app(Duration::from_millis(5));
        let create = app
            .clone()
            .oneshot(
                Request::post("/admin/v1/secrets")
                    .header("authorization", "Bearer admin-key")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{
                            "id":"openai",
                            "name":"OpenAI",
                            "source":{"provider":"environment","key":"OPENAI"},
                            "upstream":"https://api.openai.com",
                            "rules":[{"methods":["POST"],"path_prefixes":["/v1/"]}],
                            "delivery":{"type":"inject_header","header":"authorization","prefix":"Bearer "},
                            "guest":{"base_url_env":"OPENAI_BASE_URL"}
                        }"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(create.status(), StatusCode::CREATED);
        let created = response_json::<crate::SecretView>(create).await;
        assert_eq!(created.id, "openai");

        let grant = app
            .clone()
            .oneshot(
                Request::put("/admin/v1/principals/test/secrets/openai")
                    .header("authorization", "Bearer admin-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(grant.status(), StatusCode::NO_CONTENT);
        let effective = app
            .oneshot(
                Request::get("/admin/v1/principals/test/effective-secrets")
                    .header("authorization", "Bearer admin-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let effective = response_json::<Vec<crate::SecretView>>(effective).await;
        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0].source.key, "OPENAI");
    }
}

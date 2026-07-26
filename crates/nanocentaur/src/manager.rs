#![allow(clippy::missing_errors_doc)]

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::Arc,
};

use chrono::{DateTime, Utc};
use nanocodex::{AgentEventKind, ImageDetail, Prompt, SessionSnapshot, UserInput};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{RwLock, broadcast, mpsc, oneshot};
use url::Url;
use uuid::Uuid;

use crate::{
    AgentCapabilities, AgentError, AgentIdentity, AgentRunResult, AgentSpec, ManagedAgent,
    ManagedAgentFactory, ManagedTurnControl, RuntimeEvent,
    session::{CompletedTurn, NewTurn, SessionError, SessionStore, StoredSession},
};

const MAX_CONTENT_BYTES: usize = 1024 * 1024;
const MAX_CONTENT_BLOCKS: usize = 64;
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 256;
const EVENT_BROADCAST_CAPACITY: usize = 1_024;
const AGENT_COMMAND_CAPACITY: usize = 64;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CreateAgent {
    pub context_key: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CreateAgentResponse {
    pub agent_id: String,
    pub created: bool,
    pub state: AgentStatus,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentView {
    pub agent_id: String,
    pub state: AgentStatus,
    pub queue_depth: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_key: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Running,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnDelivery {
    #[default]
    Steer,
    Enqueue,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTurn {
    #[serde(default)]
    pub delivery: TurnDelivery,
    pub content: Vec<ContentBlock>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ImageUrl {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<ImageDetail>,
    },
    AudioUrl {
        url: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnAction {
    Started,
    Steered,
    Queued,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TurnActionResponse {
    pub action: TurnAction,
    pub turn_id: String,
    pub state: TurnStatus,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TurnView {
    pub turn_id: String,
    pub agent_id: String,
    pub state: TurnStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output: Vec<ContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TurnStatus {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ForkResponse {
    pub agent_id: String,
    pub forked_from: ForkSource,
    pub state: AgentStatus,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ForkSource {
    pub agent_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentEvent {
    pub id: u64,
    pub agent_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(flatten)]
    pub payload: AgentEventPayload,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum AgentEventPayload {
    #[serde(rename = "turn.started")]
    TurnStarted { state: TurnStatus },
    #[serde(rename = "turn.queued")]
    TurnQueued { state: TurnStatus },
    #[serde(rename = "turn.cancel_requested")]
    TurnCancelRequested,
    #[serde(rename = "turn.interrupted")]
    TurnInterrupted { retrying: bool },
    #[serde(rename = "turn.completed")]
    TurnCompleted { output: Vec<ContentBlock> },
    #[serde(rename = "turn.cancelled")]
    TurnCancelled,
    #[serde(rename = "turn.failed")]
    TurnFailed { error: TurnFailure },
    #[serde(rename = "runtime")]
    Runtime { event: nanocodex::AgentEvent },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnFailure {
    ManagedAgentExecutionFailed,
}

impl AgentEventPayload {
    #[must_use]
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::TurnStarted { .. } => "turn.started",
            Self::TurnQueued { .. } => "turn.queued",
            Self::TurnCancelRequested => "turn.cancel_requested",
            Self::TurnInterrupted { .. } => "turn.interrupted",
            Self::TurnCompleted { .. } => "turn.completed",
            Self::TurnCancelled => "turn.cancelled",
            Self::TurnFailed { .. } => "turn.failed",
            Self::Runtime { event } => runtime_event_name(event.kind),
        }
    }
}

/// Shared registry of lightweight actor handles. `SQLite` owns durable session
/// state; each actor is a disposable live projection that owns one harness.
pub struct AgentManager {
    factory: Arc<dyn ManagedAgentFactory>,
    agents: RwLock<HashMap<String, AgentHandle>>,
    sessions: SessionStore,
    state_directory: PathBuf,
}

impl AgentManager {
    pub fn new(
        factory: Arc<dyn ManagedAgentFactory>,
        state_directory: impl Into<PathBuf>,
    ) -> Result<Self, ManagerError> {
        let state_directory = state_directory.into();
        let sessions = SessionStore::open(state_directory.join("sessions.sqlite"))?;
        Ok(Self {
            factory,
            agents: RwLock::new(HashMap::new()),
            sessions,
            state_directory,
        })
    }

    pub async fn register(&self, identity: AgentIdentity) -> Result<AgentView, ManagerError> {
        self.ensure(identity).await?.view().await
    }

    pub async fn get(&self, identity: AgentIdentity) -> Result<AgentView, ManagerError> {
        self.ensure(identity).await?.view().await
    }

    pub async fn find_turn_by_idempotency_key(
        &self,
        identity: AgentIdentity,
        key: &str,
    ) -> Result<Option<TurnActionResponse>, ManagerError> {
        validate_idempotency_key(Some(key))?;
        self.sessions
            .find_request(identity.id, key.to_owned())
            .await
            .map_err(Into::into)
    }

    pub async fn create_turn(
        &self,
        identity: AgentIdentity,
        request: CreateTurn,
        idempotency_key: Option<String>,
    ) -> Result<TurnActionResponse, ManagerError> {
        validate_content(&request.content)?;
        validate_idempotency_key(idempotency_key.as_deref())?;
        self.ensure(identity)
            .await?
            .create_turn(request, idempotency_key)
            .await
    }

    pub async fn get_turn(
        &self,
        identity: AgentIdentity,
        turn_id: &str,
    ) -> Result<TurnView, ManagerError> {
        self.ensure(identity)
            .await?
            .get_turn(turn_id.to_owned())
            .await
    }

    pub async fn cancel_turn(
        &self,
        identity: AgentIdentity,
        turn_id: &str,
    ) -> Result<bool, ManagerError> {
        self.ensure(identity)
            .await?
            .cancel(turn_id.to_owned())
            .await
    }

    pub async fn evict(&self, identity: AgentIdentity) -> Result<bool, ManagerError> {
        self.ensure(identity).await?.evict().await
    }

    pub async fn events(
        &self,
        identity: AgentIdentity,
        after_event_id: u64,
    ) -> Result<EventCursor, ManagerError> {
        self.ensure(identity).await?.subscribe(after_event_id).await
    }

    pub async fn fork(
        &self,
        source_identity: AgentIdentity,
        target_identity: AgentIdentity,
        turn_id: Option<&str>,
    ) -> Result<ForkResponse, ManagerError> {
        let forked_from = self
            .sessions
            .fork(
                source_identity.id,
                target_identity.id.clone(),
                turn_id.map(str::to_owned),
            )
            .await?;
        let target_id = target_identity.id.clone();
        self.ensure(target_identity).await?;
        Ok(ForkResponse {
            agent_id: target_id,
            forked_from,
            state: AgentStatus::Idle,
        })
    }

    pub async fn delete(&self, agent_id: &str) -> Result<(), ManagerError> {
        let handle = self.agents.write().await.remove(agent_id);
        if let Some(handle) = handle {
            handle.shutdown().await?;
        }
        self.sessions.delete(agent_id.to_owned()).await?;
        let directory = self.state_directory.join("workspaces").join(agent_id);
        match std::fs::remove_dir_all(directory) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Stops every live harness without deleting its durable `SQLite` session.
    /// A later manager can wake the same agents from the event log.
    pub async fn shutdown(&self) -> Result<(), ManagerError> {
        let handles = self
            .agents
            .write()
            .await
            .drain()
            .map(|(_, handle)| handle)
            .collect::<Vec<_>>();
        for handle in handles {
            handle.shutdown().await?;
        }
        Ok(())
    }

    async fn ensure(&self, identity: AgentIdentity) -> Result<AgentHandle, ManagerError> {
        let existing = self.agents.read().await.get(&identity.id).cloned();
        if let Some(handle) = existing {
            handle
                .refresh_policy(
                    identity.principal.permissions,
                    identity.principal.secret_revision,
                )
                .await?;
            return Ok(handle);
        }
        self.sessions.ensure_agent(identity.id.clone()).await?;
        let stored = self.sessions.load(identity.id.clone()).await?;
        let capabilities = identity.principal.permissions.clone();
        let secret_revision = identity.principal.secret_revision;
        let handle = {
            let mut agents = self.agents.write().await;
            if let Some(handle) = agents.get(&identity.id).cloned() {
                handle
            } else {
                let id = identity.id.clone();
                let (sender, receiver) = mpsc::channel(AGENT_COMMAND_CAPACITY);
                let handle = AgentHandle {
                    sender: sender.clone(),
                };
                let actor = AgentActor::new(
                    identity,
                    Arc::clone(&self.factory),
                    self.sessions.clone(),
                    stored,
                    sender,
                );
                tokio::spawn(actor.run(receiver));
                agents.insert(id, handle.clone());
                handle
            }
        };
        handle.refresh_policy(capabilities, secret_revision).await?;
        Ok(handle)
    }
}

#[derive(Clone)]
struct AgentHandle {
    sender: mpsc::Sender<AgentCommand>,
}

impl AgentHandle {
    async fn view(&self) -> Result<AgentView, ManagerError> {
        self.request(|reply| AgentCommand::View { reply }).await
    }

    async fn refresh_policy(
        &self,
        capabilities: AgentCapabilities,
        secret_revision: u64,
    ) -> Result<(), ManagerError> {
        self.request(|reply| AgentCommand::RefreshPolicy {
            capabilities,
            secret_revision,
            reply,
        })
        .await
    }

    async fn create_turn(
        &self,
        request: CreateTurn,
        idempotency_key: Option<String>,
    ) -> Result<TurnActionResponse, ManagerError> {
        self.request(|reply| AgentCommand::CreateTurn {
            request,
            idempotency_key,
            reply,
        })
        .await
    }

    async fn get_turn(&self, turn_id: String) -> Result<TurnView, ManagerError> {
        self.request(|reply| AgentCommand::GetTurn { turn_id, reply })
            .await
    }

    async fn cancel(&self, turn_id: String) -> Result<bool, ManagerError> {
        self.request(|reply| AgentCommand::Cancel { turn_id, reply })
            .await
    }

    async fn evict(&self) -> Result<bool, ManagerError> {
        self.request(|reply| AgentCommand::Evict { reply }).await
    }

    async fn subscribe(&self, after_event_id: u64) -> Result<EventCursor, ManagerError> {
        let subscription = self
            .request(|reply| AgentCommand::Subscribe {
                after_event_id,
                reply,
            })
            .await?;
        Ok(EventCursor {
            sender: self.sender.clone(),
            receiver: subscription.receiver,
            after_event_id,
            replay: subscription.replay,
        })
    }

    async fn shutdown(&self) -> Result<(), ManagerError> {
        self.request(|reply| AgentCommand::Shutdown { reply }).await
    }

    async fn request<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<T, ManagerError>>) -> AgentCommand,
    ) -> Result<T, ManagerError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(command(reply))
            .await
            .map_err(|_| ManagerError::ActorStopped)?;
        response.await.map_err(|_| ManagerError::ActorStopped)?
    }
}

enum AgentCommand {
    View {
        reply: Reply<AgentView>,
    },
    RefreshPolicy {
        capabilities: AgentCapabilities,
        secret_revision: u64,
        reply: Reply<()>,
    },
    CreateTurn {
        request: CreateTurn,
        idempotency_key: Option<String>,
        reply: Reply<TurnActionResponse>,
    },
    GetTurn {
        turn_id: String,
        reply: Reply<TurnView>,
    },
    Cancel {
        turn_id: String,
        reply: Reply<bool>,
    },
    Evict {
        reply: Reply<bool>,
    },
    Subscribe {
        after_event_id: u64,
        reply: Reply<EventSubscription>,
    },
    Replay {
        after_event_id: u64,
        reply: Reply<VecDeque<AgentEvent>>,
    },
    RuntimeEvent(RuntimeEvent),
    TurnFinished {
        turn_id: String,
        result: Box<Result<AgentRunResult, AgentError>>,
    },
    FinalizeTurn {
        turn_id: String,
    },
    Shutdown {
        reply: Reply<()>,
    },
}

type Reply<T> = oneshot::Sender<Result<T, ManagerError>>;

struct Runtime {
    agent: Arc<dyn ManagedAgent>,
}

struct StoredTurn {
    view: TurnView,
    inputs: Vec<Vec<ContentBlock>>,
    control: Option<Arc<dyn ManagedTurnControl>>,
    snapshot: Option<SessionSnapshot>,
}

struct EventSubscription {
    receiver: broadcast::Receiver<AgentEvent>,
    replay: VecDeque<AgentEvent>,
}

struct AgentActor {
    id: String,
    principal_id: String,
    context_key: Option<String>,
    instructions: Option<String>,
    thinking: Option<nanocodex::Thinking>,
    capabilities: AgentCapabilities,
    secret_revision: u64,
    created_at: DateTime<Utc>,
    factory: Arc<dyn ManagedAgentFactory>,
    sessions: SessionStore,
    self_sender: mpsc::Sender<AgentCommand>,
    active_turn: Option<String>,
    runtime_event_turn: Option<String>,
    runtime_terminal_turns: HashSet<String>,
    pending_results: HashMap<String, Result<AgentRunResult, AgentError>>,
    cancel_requested: HashSet<String>,
    turns: HashMap<String, StoredTurn>,
    turn_order: VecDeque<String>,
    requests_by_key: HashMap<String, TurnActionResponse>,
    runtime: Option<Runtime>,
    runtime_policy_dirty: bool,
    snapshot: Option<SessionSnapshot>,
    event_sender: broadcast::Sender<AgentEvent>,
}

impl AgentActor {
    fn new(
        identity: AgentIdentity,
        factory: Arc<dyn ManagedAgentFactory>,
        sessions: SessionStore,
        stored: StoredSession,
        self_sender: mpsc::Sender<AgentCommand>,
    ) -> Self {
        let (event_sender, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let agent_config = identity.principal.agent_config;
        let mut turns = HashMap::new();
        let mut turn_order = VecDeque::new();
        let mut cancel_requested = HashSet::new();
        let mut snapshot = None;
        for turn in stored.turns {
            if turn.cancel_requested {
                cancel_requested.insert(turn.view.turn_id.clone());
            }
            if turn.view.state == TurnStatus::Completed {
                snapshot.clone_from(&turn.snapshot);
            }
            turn_order.push_back(turn.view.turn_id.clone());
            turns.insert(
                turn.view.turn_id.clone(),
                StoredTurn {
                    view: turn.view,
                    inputs: turn.inputs,
                    control: None,
                    snapshot: turn.snapshot,
                },
            );
        }
        Self {
            id: identity.id,
            principal_id: identity.principal.id,
            context_key: identity.context_key,
            instructions: agent_config.instructions,
            thinking: agent_config.reasoning_effort.map(Into::into),
            capabilities: identity.principal.permissions,
            secret_revision: identity.principal.secret_revision,
            created_at: identity.created_at,
            factory,
            sessions,
            self_sender,
            active_turn: None,
            runtime_event_turn: None,
            runtime_terminal_turns: HashSet::new(),
            pending_results: HashMap::new(),
            cancel_requested,
            turns,
            turn_order,
            requests_by_key: stored.requests,
            runtime: None,
            runtime_policy_dirty: false,
            snapshot,
            event_sender,
        }
    }

    async fn run(mut self, mut receiver: mpsc::Receiver<AgentCommand>) {
        if let Err(error) = self.recover_pending().await {
            tracing::error!(agent_id = self.id, %error, "failed to recover durable agent session");
        }
        while let Some(command) = receiver.recv().await {
            let should_stop = matches!(command, AgentCommand::Shutdown { .. });
            self.handle(command).await;
            if should_stop {
                return;
            }
        }
    }

    async fn handle(&mut self, command: AgentCommand) {
        match command {
            AgentCommand::View { reply } => {
                drop(reply.send(Ok(self.view())));
            }
            AgentCommand::RefreshPolicy {
                capabilities,
                secret_revision,
                reply,
            } => {
                self.apply_policy(capabilities, secret_revision);
                drop(reply.send(Ok(())));
            }
            AgentCommand::CreateTurn {
                request,
                idempotency_key,
                reply,
            } => {
                let result = self.create_turn(request, idempotency_key).await;
                drop(reply.send(result));
            }
            AgentCommand::GetTurn { turn_id, reply } => {
                let result = self
                    .turns
                    .get(&turn_id)
                    .map(|turn| turn.view.clone())
                    .ok_or(ManagerError::NotFound);
                drop(reply.send(result));
            }
            AgentCommand::Cancel { turn_id, reply } => {
                let result = self.cancel(&turn_id).await;
                drop(reply.send(result));
            }
            AgentCommand::Evict { reply } => {
                let result = if self.active_turn.is_some() {
                    Ok(false)
                } else {
                    Ok(self.runtime.take().is_some())
                };
                drop(reply.send(result));
            }
            AgentCommand::Subscribe {
                after_event_id,
                reply,
            } => {
                let receiver = self.event_sender.subscribe();
                let replay = self
                    .sessions
                    .events_after(self.id.clone(), after_event_id)
                    .await
                    .map(VecDeque::from)
                    .map_err(ManagerError::from);
                drop(reply.send(replay.map(|replay| EventSubscription { receiver, replay })));
            }
            AgentCommand::Replay {
                after_event_id,
                reply,
            } => {
                let replay = self
                    .sessions
                    .events_after(self.id.clone(), after_event_id)
                    .await
                    .map(VecDeque::from)
                    .map_err(ManagerError::from);
                drop(reply.send(replay));
            }
            AgentCommand::RuntimeEvent(event) => {
                self.handle_runtime_event(event).await;
            }
            AgentCommand::TurnFinished { turn_id, result } => {
                if self.runtime_terminal_turns.remove(&turn_id) {
                    self.finish_turn(&turn_id, *result).await;
                } else {
                    self.pending_results.insert(turn_id.clone(), *result);
                    let sender = self.self_sender.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        drop(sender.send(AgentCommand::FinalizeTurn { turn_id }).await);
                    });
                }
            }
            AgentCommand::FinalizeTurn { turn_id } => {
                if let Some(result) = self.pending_results.remove(&turn_id) {
                    self.finish_turn(&turn_id, result).await;
                }
            }
            AgentCommand::Shutdown { reply } => {
                self.runtime.take();
                drop(reply.send(Ok(())));
            }
        }
    }

    fn apply_policy(&mut self, capabilities: AgentCapabilities, secret_revision: u64) {
        if self.capabilities == capabilities && self.secret_revision == secret_revision {
            return;
        }
        self.capabilities = capabilities;
        self.secret_revision = secret_revision;
        if self.active_turn.is_some() {
            self.runtime_policy_dirty = true;
        } else {
            self.runtime.take();
            self.runtime_policy_dirty = false;
        }
    }

    fn view(&self) -> AgentView {
        AgentView {
            agent_id: self.id.clone(),
            state: if self.active_turn.is_some() {
                AgentStatus::Running
            } else {
                AgentStatus::Idle
            },
            queue_depth: self
                .turns
                .values()
                .filter(|turn| turn.view.state == TurnStatus::Queued)
                .count(),
            context_key: self.context_key.clone(),
            created_at: self.created_at,
        }
    }

    async fn create_turn(
        &mut self,
        request: CreateTurn,
        idempotency_key: Option<String>,
    ) -> Result<TurnActionResponse, ManagerError> {
        if let Some(key) = &idempotency_key
            && let Some(response) = self.requests_by_key.get(key)
        {
            return Ok(response.clone());
        }

        let content = request.content;
        let prompt = to_prompt(content.clone());
        if request.delivery == TurnDelivery::Steer
            && let Some(response) = self
                .steer_active(prompt.clone(), content.clone(), idempotency_key.clone())
                .await?
        {
            return Ok(response);
        }

        self.ensure_runtime().await?;
        let managed = self
            .runtime
            .as_ref()
            .map(|runtime| Arc::clone(&runtime.agent))
            .ok_or_else(|| AgentError::Backend("runtime disappeared".to_owned()))?;
        let turn_id = Uuid::new_v4().to_string();
        let (action, state) = if self.active_turn.is_some() {
            (TurnAction::Queued, TurnStatus::Queued)
        } else {
            self.active_turn = Some(turn_id.clone());
            if self.runtime_event_turn.is_none() {
                self.runtime_event_turn = Some(turn_id.clone());
            }
            (TurnAction::Started, TurnStatus::Running)
        };
        let view = TurnView {
            turn_id: turn_id.clone(),
            agent_id: self.id.clone(),
            state,
            output: Vec::new(),
            error: None,
            created_at: Utc::now(),
            completed_at: None,
        };
        let response = TurnActionResponse {
            action,
            turn_id: turn_id.clone(),
            state,
        };
        let event_payload = match action {
            TurnAction::Started => AgentEventPayload::TurnStarted { state },
            TurnAction::Queued => AgentEventPayload::TurnQueued { state },
            TurnAction::Steered => unreachable!(),
        };
        let event = self
            .sessions
            .record_turn(
                self.id.clone(),
                NewTurn {
                    view: view.clone(),
                    delivery: request.delivery,
                    content: content.clone(),
                    response: response.clone(),
                    idempotency_key: idempotency_key.clone(),
                    event: event_payload,
                },
            )
            .await?;
        self.publish(event);
        self.turn_order.push_back(turn_id.clone());
        self.turns.insert(
            turn_id.clone(),
            StoredTurn {
                view,
                inputs: vec![content],
                control: None,
                snapshot: None,
            },
        );
        self.remember_request(idempotency_key, &response);
        match managed.prompt(prompt).await {
            Ok(managed_turn) => self.attach_turn(turn_id, managed_turn),
            Err(error) => {
                self.finish_turn(&turn_id, Err(error)).await;
            }
        }
        Ok(response)
    }

    async fn steer_active(
        &mut self,
        prompt: Prompt,
        content: Vec<ContentBlock>,
        idempotency_key: Option<String>,
    ) -> Result<Option<TurnActionResponse>, ManagerError> {
        let Some(active_id) = self.active_turn.clone() else {
            return Ok(None);
        };
        let Some(control) = self
            .turns
            .get(&active_id)
            .and_then(|turn| turn.control.as_ref().map(Arc::clone))
        else {
            return Ok(None);
        };
        match control.steer(prompt).await {
            Ok(()) => {
                let response = TurnActionResponse {
                    action: TurnAction::Steered,
                    turn_id: active_id.clone(),
                    state: TurnStatus::Running,
                };
                self.sessions
                    .record_steer(
                        self.id.clone(),
                        active_id.clone(),
                        content.clone(),
                        response.clone(),
                        idempotency_key.clone(),
                    )
                    .await?;
                if let Some(turn) = self.turns.get_mut(&active_id) {
                    turn.inputs.push(content);
                }
                self.remember_request(idempotency_key, &response);
                Ok(Some(response))
            }
            Err(AgentError::TurnNotSteerable) => {
                self.active_turn = None;
                Ok(None)
            }
            Err(AgentError::SteerQueueFull) => Err(ManagerError::SteerQueueFull),
            Err(error) => Err(error.into()),
        }
    }

    async fn cancel(&mut self, turn_id: &str) -> Result<bool, ManagerError> {
        let control = self
            .turns
            .get(turn_id)
            .filter(|turn| !turn.view.state.is_terminal())
            .and_then(|turn| turn.control.as_ref().map(Arc::clone))
            .ok_or(ManagerError::NotFound)?;
        match control.cancel().await {
            Ok(()) => {
                self.cancel_requested.insert(turn_id.to_owned());
                let event = self
                    .sessions
                    .request_cancel(self.id.clone(), turn_id.to_owned())
                    .await?;
                self.publish(event);
                Ok(true)
            }
            Err(AgentError::TurnNotCancellable) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    async fn ensure_runtime(&mut self) -> Result<(), AgentError> {
        if self.runtime.is_some() {
            return Ok(());
        }
        let spawned = self
            .factory
            .create(AgentSpec {
                agent_id: self.id.clone(),
                principal: self.principal_id.clone(),
                instructions: self.instructions.clone(),
                thinking: self.thinking,
                capabilities: self.capabilities.clone(),
                snapshot: self.snapshot.clone(),
            })
            .await?;
        let sender = self.self_sender.clone();
        tokio::spawn(async move {
            let mut events = spawned.events;
            while let Some(event) = events.recv().await {
                if sender
                    .send(AgentCommand::RuntimeEvent(event))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
        self.runtime = Some(Runtime {
            agent: spawned.agent,
        });
        Ok(())
    }

    async fn finish_turn(&mut self, turn_id: &str, result: Result<AgentRunResult, AgentError>) {
        let cancelled = self.cancel_requested.remove(turn_id);
        let completed_at = Utc::now();
        let (status, output, error, snapshot, payload) = match result {
            Ok(result) if !cancelled => {
                self.snapshot.clone_from(&result.snapshot);
                let output = vec![ContentBlock::Text {
                    text: result.final_message,
                }];
                if let Some(turn) = self.turns.get_mut(turn_id) {
                    turn.view.state = TurnStatus::Completed;
                    turn.view.output.clone_from(&output);
                    turn.view.completed_at = Some(completed_at);
                    turn.snapshot.clone_from(&result.snapshot);
                }
                (
                    TurnStatus::Completed,
                    output.clone(),
                    None,
                    result.snapshot,
                    AgentEventPayload::TurnCompleted { output },
                )
            }
            _ if cancelled => {
                if let Some(turn) = self.turns.get_mut(turn_id) {
                    turn.view.state = TurnStatus::Cancelled;
                    turn.view.completed_at = Some(completed_at);
                }
                (
                    TurnStatus::Cancelled,
                    Vec::new(),
                    None,
                    None,
                    AgentEventPayload::TurnCancelled,
                )
            }
            Ok(_) => {
                // The guarded cancellation arm above is the only remaining
                // successful-result path.
                unreachable!("successful non-cancelled turns are handled first");
            }
            Err(error) => {
                tracing::warn!(agent_id = self.id, turn_id, %error, "turn failed");
                if let Some(turn) = self.turns.get_mut(turn_id) {
                    turn.view.state = TurnStatus::Failed;
                    turn.view.error = Some("managed agent execution failed".to_owned());
                    turn.view.completed_at = Some(completed_at);
                }
                (
                    TurnStatus::Failed,
                    Vec::new(),
                    Some("managed agent execution failed".to_owned()),
                    None,
                    AgentEventPayload::TurnFailed {
                        error: TurnFailure::ManagedAgentExecutionFailed,
                    },
                )
            }
        };
        match self
            .sessions
            .finish_turn(
                self.id.clone(),
                turn_id.to_owned(),
                CompletedTurn {
                    status,
                    output,
                    error,
                    completed_at,
                    snapshot,
                    event: payload,
                },
            )
            .await
        {
            Ok(event) => self.publish(event),
            Err(error) => {
                tracing::error!(agent_id = self.id, turn_id, %error, "failed to persist terminal turn");
                return;
            }
        }
        self.advance_after(turn_id).await;
    }

    async fn handle_runtime_event(&mut self, event: RuntimeEvent) {
        let kind = event.0.kind;
        if kind == AgentEventKind::RunStarted && self.runtime_event_turn.is_none() {
            if let Some(next) = self.turn_order.iter().find_map(|id| {
                self.turns
                    .get(id)
                    .filter(|turn| turn.view.state == TurnStatus::Queued)
                    .map(|_| id.clone())
            }) {
                self.active_turn = Some(next.clone());
                if let Some(turn) = self.turns.get_mut(&next) {
                    turn.view.state = TurnStatus::Running;
                }
                match self
                    .sessions
                    .mark_started(self.id.clone(), next.clone())
                    .await
                {
                    Ok(event) => self.publish(event),
                    Err(error) => {
                        tracing::error!(agent_id = self.id, turn_id = next, %error, "failed to persist queued turn start");
                    }
                }
                self.runtime_event_turn = Some(next);
            } else {
                self.runtime_event_turn.clone_from(&self.active_turn);
            }
        }
        let turn_id = self.runtime_event_turn.clone();
        let terminal = kind.is_terminal();
        match self
            .sessions
            .append_event(
                self.id.clone(),
                turn_id.clone(),
                AgentEventPayload::Runtime { event: event.0 },
            )
            .await
        {
            Ok(event) => self.publish(event),
            Err(error) => {
                tracing::error!(agent_id = self.id, %error, "failed to persist runtime event");
            }
        }
        if terminal {
            self.runtime_event_turn = None;
            if let Some(turn_id) = turn_id {
                if let Some(result) = self.pending_results.remove(&turn_id) {
                    self.finish_turn(&turn_id, result).await;
                } else {
                    self.runtime_terminal_turns.insert(turn_id);
                }
            }
        }
    }

    async fn advance_after(&mut self, completed_turn_id: &str) {
        if self.active_turn.as_deref() != Some(completed_turn_id) {
            return;
        }
        let next = self.turn_order.iter().find_map(|id| {
            self.turns
                .get(id)
                .filter(|turn| turn.view.state == TurnStatus::Queued)
                .map(|_| id.clone())
        });
        self.active_turn.clone_from(&next);
        if let Some(next) = next {
            if let Some(turn) = self.turns.get_mut(&next) {
                turn.view.state = TurnStatus::Running;
            }
            match self
                .sessions
                .mark_started(self.id.clone(), next.clone())
                .await
            {
                Ok(event) => self.publish(event),
                Err(error) => {
                    tracing::error!(agent_id = self.id, turn_id = next, %error, "failed to persist queued turn start");
                    return;
                }
            }
            if self.runtime_event_turn.is_none() {
                self.runtime_event_turn = Some(next);
            }
        } else if self.runtime_policy_dirty {
            self.runtime.take();
            self.runtime_policy_dirty = false;
        }
    }

    fn remember_request(&mut self, idempotency_key: Option<String>, response: &TurnActionResponse) {
        if let Some(key) = idempotency_key {
            self.requests_by_key.insert(key, response.clone());
        }
    }

    fn publish(&self, event: AgentEvent) {
        let _ = self.event_sender.send(event);
    }

    fn attach_turn(&mut self, turn_id: String, managed_turn: crate::ManagedTurn) {
        if let Some(turn) = self.turns.get_mut(&turn_id) {
            turn.control = Some(Arc::clone(&managed_turn.control));
        }
        let sender = self.self_sender.clone();
        tokio::spawn(async move {
            let result = managed_turn.result.await;
            drop(
                sender
                    .send(AgentCommand::TurnFinished {
                        turn_id,
                        result: Box::new(result),
                    })
                    .await,
            );
        });
    }

    async fn recover_pending(&mut self) -> Result<(), ManagerError> {
        let pending = self
            .turn_order
            .iter()
            .filter(|turn_id| {
                self.turns
                    .get(*turn_id)
                    .is_some_and(|turn| !turn.view.state.is_terminal())
            })
            .cloned()
            .collect::<Vec<_>>();
        if pending.is_empty() {
            return Ok(());
        }
        self.ensure_runtime().await?;
        let managed = self
            .runtime
            .as_ref()
            .map(|runtime| Arc::clone(&runtime.agent))
            .ok_or_else(|| AgentError::Backend("runtime disappeared".to_owned()))?;
        let mut started = false;
        for turn_id in pending {
            if self.cancel_requested.contains(&turn_id) {
                self.finish_turn(&turn_id, Err(AgentError::TurnNotCancellable))
                    .await;
                continue;
            }
            let inputs = self
                .turns
                .get(&turn_id)
                .map(|turn| turn.inputs.clone())
                .ok_or(ManagerError::NotFound)?;
            let Some(first) = inputs.first().cloned() else {
                self.finish_turn(
                    &turn_id,
                    Err(AgentError::Backend(
                        "durable turn has no input content".to_owned(),
                    )),
                )
                .await;
                continue;
            };
            if !started {
                started = true;
                self.active_turn = Some(turn_id.clone());
                self.runtime_event_turn = Some(turn_id.clone());
                if let Some(turn) = self.turns.get_mut(&turn_id) {
                    turn.view.state = TurnStatus::Running;
                }
                let event = self
                    .sessions
                    .mark_started(self.id.clone(), turn_id.clone())
                    .await?;
                self.publish(event);
            }
            match managed.prompt(to_prompt(first)).await {
                Ok(managed_turn) => {
                    for steer in inputs.iter().skip(1).cloned() {
                        managed_turn.control.steer(to_prompt(steer)).await?;
                    }
                    self.attach_turn(turn_id, managed_turn);
                }
                Err(error) => self.finish_turn(&turn_id, Err(error)).await,
            }
        }
        Ok(())
    }
}

pub struct EventCursor {
    sender: mpsc::Sender<AgentCommand>,
    receiver: broadcast::Receiver<AgentEvent>,
    after_event_id: u64,
    replay: VecDeque<AgentEvent>,
}

impl EventCursor {
    pub async fn next(&mut self) -> Option<AgentEvent> {
        loop {
            if let Some(event) = self.replay.pop_front() {
                self.after_event_id = event.id;
                return Some(event);
            }
            match self.receiver.recv().await {
                Ok(event) if event.id > self.after_event_id => {
                    self.after_event_id = event.id;
                    return Some(event);
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let (reply, response) = oneshot::channel();
                    if self
                        .sender
                        .send(AgentCommand::Replay {
                            after_event_id: self.after_event_id,
                            reply,
                        })
                        .await
                        .is_err()
                    {
                        return None;
                    }
                    self.replay = response.await.ok()?.ok()?;
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

const fn runtime_event_name(kind: AgentEventKind) -> &'static str {
    match kind {
        AgentEventKind::ApiEvent => "api.event",
        AgentEventKind::AssistantDelta => "assistant.delta",
        AgentEventKind::AssistantMessage => "assistant.message",
        AgentEventKind::ReasoningSummaryDelta => "reasoning.summary.delta",
        AgentEventKind::RunStarted => "run.started",
        AgentEventKind::RunSteered => "run.steered",
        AgentEventKind::RunError => "run.error",
        AgentEventKind::RunCompleted => "run.completed",
        AgentEventKind::RunFailed => "run.failed",
        AgentEventKind::ToolCall => "tool.call",
        AgentEventKind::ToolResult => "tool.result",
        AgentEventKind::ModelWarmupStarted => "model.warmup.started",
        AgentEventKind::ModelWarmupCompleted => "model.warmup.completed",
        AgentEventKind::ModelWarmupFailed => "model.warmup.failed",
        AgentEventKind::ModelCallStarted => "model.call.started",
        AgentEventKind::ModelCallCompleted => "model.call.completed",
        AgentEventKind::ModelCallFailed => "model.call.failed",
        AgentEventKind::ModelCompactionStarted => "model.compaction.started",
        AgentEventKind::ModelCompactionCompleted => "model.compaction.completed",
        AgentEventKind::ModelCompactionFailed => "model.compaction.failed",
        AgentEventKind::ModelAttemptStarted => "model.attempt.started",
        AgentEventKind::ModelAttemptFailed => "model.attempt.failed",
        AgentEventKind::ModelAttemptRetrying => "model.attempt.retrying",
        AgentEventKind::ModelConnectionStarted => "model.connection.started",
        AgentEventKind::ModelConnectionCompleted => "model.connection.completed",
        AgentEventKind::ModelConnectionFailed => "model.connection.failed",
    }
}

fn to_prompt(content: Vec<ContentBlock>) -> Prompt {
    Prompt::content(content.into_iter().map(|block| match block {
        ContentBlock::Text { text } => UserInput::Text { text },
        ContentBlock::ImageUrl { url, detail } => UserInput::Image {
            image_url: url,
            detail,
        },
        ContentBlock::AudioUrl { url } => UserInput::Audio { audio_url: url },
    }))
}

fn validate_content(content: &[ContentBlock]) -> Result<(), ManagerError> {
    if content.is_empty() || content.len() > MAX_CONTENT_BLOCKS {
        return Err(ManagerError::Invalid("content must contain 1 to 64 blocks"));
    }
    let encoded = serde_json::to_vec(content)
        .map_err(|_| ManagerError::Invalid("content must be valid JSON"))?;
    if encoded.len() > MAX_CONTENT_BYTES {
        return Err(ManagerError::Invalid(
            "content must not exceed 1048576 encoded bytes",
        ));
    }
    for block in content {
        match block {
            ContentBlock::Text { text } if text.trim().is_empty() => {
                return Err(ManagerError::Invalid("text blocks must not be empty"));
            }
            ContentBlock::ImageUrl { url, .. } | ContentBlock::AudioUrl { url } => {
                let url = Url::parse(url).map_err(|_| {
                    ManagerError::Invalid("media URLs must be absolute HTTP(S) URLs")
                })?;
                if !matches!(url.scheme(), "http" | "https") {
                    return Err(ManagerError::Invalid(
                        "media URLs must be absolute HTTP(S) URLs",
                    ));
                }
            }
            ContentBlock::Text { .. } => {}
        }
    }
    Ok(())
}

fn validate_idempotency_key(value: Option<&str>) -> Result<(), ManagerError> {
    if value.is_some_and(|value| value.is_empty() || value.len() > MAX_IDEMPOTENCY_KEY_BYTES) {
        Err(ManagerError::Invalid(
            "Idempotency-Key must contain 1 to 256 bytes",
        ))
    } else {
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ManagerError {
    #[error("agent or turn was not found")]
    NotFound,
    #[error("invalid request: {0}")]
    Invalid(&'static str),
    #[error("the active turn's steering queue is full")]
    SteerQueueFull,
    #[error("agent must be idle before it can be forked")]
    AgentBusy,
    #[error("completed fork boundary was not found")]
    ForkBoundaryNotFound,
    #[error("agent actor stopped")]
    ActorStopped,
    #[error("session durability failed: {0}")]
    Durability(String),
    #[error("managed agent could not be created or controlled")]
    Agent(#[from] AgentError),
    #[error("agent state filesystem failed")]
    Io(#[from] std::io::Error),
}

impl From<SessionError> for ManagerError {
    fn from(error: SessionError) -> Self {
        match error {
            SessionError::ForkBoundaryNotFound => Self::ForkBoundaryNotFound,
            error => Self::Durability(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::{AgentConfig, EffectivePrincipal, MockAgentFactory};

    fn identity(id: &str) -> AgentIdentity {
        AgentIdentity {
            id: id.to_owned(),
            owner_client_id: "client".to_owned(),
            context_key: None,
            principal: EffectivePrincipal {
                id: "principal".to_owned(),
                agent_config: AgentConfig::default(),
                permissions: AgentCapabilities::default(),
                secret_revision: 0,
            },
            created_at: Utc::now(),
        }
    }

    fn turn(text: &str) -> CreateTurn {
        CreateTurn {
            delivery: TurnDelivery::Steer,
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
        }
    }

    async fn wait_for_completed(
        manager: &AgentManager,
        identity: AgentIdentity,
        turn_id: &str,
    ) -> TurnView {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let view = manager.get_turn(identity.clone(), turn_id).await.unwrap();
                if view.state.is_terminal() {
                    return view;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn agent_actor_owns_one_ordered_typed_event_stream() {
        let directory = tempfile::tempdir().unwrap();
        let manager = AgentManager::new(
            Arc::new(MockAgentFactory::new(Duration::from_millis(5))),
            directory.path(),
        )
        .unwrap();
        let identity = identity("agent");
        manager.register(identity.clone()).await.unwrap();
        let mut events = manager.events(identity.clone(), 0).await.unwrap();
        manager
            .create_turn(
                identity,
                CreateTurn {
                    delivery: TurnDelivery::Steer,
                    content: vec![ContentBlock::Text {
                        text: "hello".to_owned(),
                    }],
                },
                None,
            )
            .await
            .unwrap();

        let mut saw_runtime = false;
        loop {
            let event = tokio::time::timeout(Duration::from_secs(1), events.next())
                .await
                .unwrap()
                .unwrap();
            match event.payload {
                AgentEventPayload::Runtime { .. } => saw_runtime = true,
                AgentEventPayload::TurnCompleted { .. } => break,
                AgentEventPayload::TurnStarted { .. }
                | AgentEventPayload::TurnQueued { .. }
                | AgentEventPayload::TurnCancelRequested
                | AgentEventPayload::TurnInterrupted { .. }
                | AgentEventPayload::TurnCancelled
                | AgentEventPayload::TurnFailed { .. } => {}
            }
        }
        assert!(saw_runtime);
    }

    #[tokio::test]
    async fn completed_turns_events_and_idempotency_survive_restart() {
        let directory = tempfile::tempdir().unwrap();
        let identity = identity("durable");
        let manager = AgentManager::new(
            Arc::new(MockAgentFactory::new(Duration::from_millis(5))),
            directory.path(),
        )
        .unwrap();
        manager.register(identity.clone()).await.unwrap();
        let response = manager
            .create_turn(
                identity.clone(),
                turn("persist me"),
                Some("request-1".to_owned()),
            )
            .await
            .unwrap();
        let completed = wait_for_completed(&manager, identity.clone(), &response.turn_id).await;
        assert_eq!(completed.state, TurnStatus::Completed);
        let mut first_events = manager.events(identity.clone(), 0).await.unwrap();
        let terminal_id = loop {
            let event = first_events.next().await.unwrap();
            if matches!(event.payload, AgentEventPayload::TurnCompleted { .. }) {
                break event.id;
            }
        };
        manager.shutdown().await.unwrap();
        drop(manager);

        let restarted = AgentManager::new(
            Arc::new(MockAgentFactory::new(Duration::from_millis(5))),
            directory.path(),
        )
        .unwrap();
        restarted.register(identity.clone()).await.unwrap();
        let restored = restarted
            .get_turn(identity.clone(), &response.turn_id)
            .await
            .unwrap();
        assert_eq!(restored.state, TurnStatus::Completed);
        assert_eq!(
            restarted
                .find_turn_by_idempotency_key(identity.clone(), "request-1")
                .await
                .unwrap()
                .unwrap()
                .turn_id,
            response.turn_id
        );
        let mut replay = restarted.events(identity, 0).await.unwrap();
        let last = loop {
            let event = replay.next().await.unwrap();
            if event.id == terminal_id {
                break event;
            }
        };
        assert!(matches!(
            last.payload,
            AgentEventPayload::TurnCompleted { .. }
        ));
    }

    #[tokio::test]
    async fn interrupted_running_turn_is_retried_from_sqlite() {
        let directory = tempfile::tempdir().unwrap();
        let identity = identity("interrupted");
        let manager = AgentManager::new(
            Arc::new(MockAgentFactory::new(Duration::from_secs(1))),
            directory.path(),
        )
        .unwrap();
        manager.register(identity.clone()).await.unwrap();
        let response = manager
            .create_turn(identity.clone(), turn("retry me"), None)
            .await
            .unwrap();
        manager.shutdown().await.unwrap();
        drop(manager);

        let restarted = AgentManager::new(
            Arc::new(MockAgentFactory::new(Duration::from_millis(5))),
            directory.path(),
        )
        .unwrap();
        restarted.register(identity.clone()).await.unwrap();
        let completed = wait_for_completed(&restarted, identity.clone(), &response.turn_id).await;
        assert_eq!(completed.state, TurnStatus::Completed);
        let mut events = restarted.events(identity, 0).await.unwrap();
        let mut interrupted = false;
        loop {
            match events.next().await.unwrap().payload {
                AgentEventPayload::TurnInterrupted { retrying } => interrupted = retrying,
                AgentEventPayload::TurnCompleted { .. } => break,
                AgentEventPayload::TurnStarted { .. }
                | AgentEventPayload::TurnQueued { .. }
                | AgentEventPayload::TurnCancelRequested
                | AgentEventPayload::TurnCancelled
                | AgentEventPayload::TurnFailed { .. }
                | AgentEventPayload::Runtime { .. } => {}
            }
        }
        assert!(interrupted);
    }

    #[tokio::test]
    async fn fork_copies_session_history_without_copying_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let source = identity("source");
        let target = identity("target");
        let manager = AgentManager::new(
            Arc::new(MockAgentFactory::new(Duration::from_millis(5))),
            directory.path(),
        )
        .unwrap();
        manager.register(source.clone()).await.unwrap();
        let response = manager
            .create_turn(source.clone(), turn("branch here"), None)
            .await
            .unwrap();
        wait_for_completed(&manager, source.clone(), &response.turn_id).await;
        let source_workspace = directory.path().join("workspaces").join("source");
        std::fs::create_dir_all(&source_workspace).unwrap();
        std::fs::write(source_workspace.join("pet.txt"), "do not clone").unwrap();

        let forked = manager
            .fork(source, target.clone(), Some(response.turn_id.as_str()))
            .await
            .unwrap();
        assert_eq!(
            forked.forked_from.turn_id.as_deref(),
            Some(response.turn_id.as_str())
        );
        let inherited = manager
            .get_turn(target.clone(), &response.turn_id)
            .await
            .unwrap();
        assert_eq!(inherited.state, TurnStatus::Completed);
        assert!(
            !directory
                .path()
                .join("workspaces")
                .join("target")
                .join("pet.txt")
                .exists()
        );
        let first = manager
            .events(target, 0)
            .await
            .unwrap()
            .next()
            .await
            .unwrap();
        assert_eq!(first.agent_id, "target");
    }
}

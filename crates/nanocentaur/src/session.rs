use std::{collections::HashMap, path::Path};

use chrono::{DateTime, Utc};
use nanocodex::SessionSnapshot;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use crate::{
    AgentEvent, AgentEventPayload, ContentBlock, ForkSource, TurnAction, TurnActionResponse,
    TurnDelivery, TurnStatus, TurnView,
};

const COMMAND_CAPACITY: usize = 256;

#[derive(Clone)]
pub(crate) struct SessionStore {
    sender: mpsc::Sender<Command>,
}

pub(crate) struct StoredSession {
    pub turns: Vec<StoredTurn>,
    pub requests: HashMap<String, TurnActionResponse>,
}

pub(crate) struct StoredTurn {
    pub view: TurnView,
    pub inputs: Vec<Vec<ContentBlock>>,
    pub cancel_requested: bool,
    pub snapshot: Option<SessionSnapshot>,
}

pub(crate) struct NewTurn {
    pub view: TurnView,
    pub delivery: TurnDelivery,
    pub content: Vec<ContentBlock>,
    pub response: TurnActionResponse,
    pub idempotency_key: Option<String>,
    pub event: AgentEventPayload,
}

pub(crate) struct CompletedTurn {
    pub status: TurnStatus,
    pub output: Vec<ContentBlock>,
    pub error: Option<String>,
    pub completed_at: DateTime<Utc>,
    pub snapshot: Option<SessionSnapshot>,
    pub event: AgentEventPayload,
}

impl SessionStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SessionError> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        configure(&connection)?;
        let (sender, mut receiver) = mpsc::channel::<Command>(COMMAND_CAPACITY);
        tokio::task::spawn_blocking(move || {
            while let Some(command) = receiver.blocking_recv() {
                command.execute(&connection);
            }
        });
        Ok(Self { sender })
    }

    pub async fn ensure_agent(&self, agent_id: String) -> Result<(), SessionError> {
        self.call(|reply| Command::EnsureAgent { agent_id, reply })
            .await
    }

    pub async fn load(&self, agent_id: String) -> Result<StoredSession, SessionError> {
        self.call(|reply| Command::Load { agent_id, reply }).await
    }

    pub async fn find_request(
        &self,
        agent_id: String,
        key: String,
    ) -> Result<Option<TurnActionResponse>, SessionError> {
        self.call(|reply| Command::FindRequest {
            agent_id,
            key,
            reply,
        })
        .await
    }

    pub async fn record_turn(
        &self,
        agent_id: String,
        turn: NewTurn,
    ) -> Result<AgentEvent, SessionError> {
        self.call(|reply| Command::RecordTurn {
            agent_id,
            turn: Box::new(turn),
            reply,
        })
        .await
    }

    pub async fn record_steer(
        &self,
        agent_id: String,
        turn_id: String,
        content: Vec<ContentBlock>,
        response: TurnActionResponse,
        idempotency_key: Option<String>,
    ) -> Result<(), SessionError> {
        self.call(|reply| Command::RecordSteer {
            agent_id,
            turn_id,
            content,
            response,
            idempotency_key,
            reply,
        })
        .await
    }

    pub async fn mark_started(
        &self,
        agent_id: String,
        turn_id: String,
    ) -> Result<AgentEvent, SessionError> {
        self.call(|reply| Command::MarkStarted {
            agent_id,
            turn_id,
            reply,
        })
        .await
    }

    pub async fn request_cancel(
        &self,
        agent_id: String,
        turn_id: String,
    ) -> Result<AgentEvent, SessionError> {
        self.call(|reply| Command::RequestCancel {
            agent_id,
            turn_id,
            reply,
        })
        .await
    }

    pub async fn finish_turn(
        &self,
        agent_id: String,
        turn_id: String,
        completed: CompletedTurn,
    ) -> Result<AgentEvent, SessionError> {
        self.call(|reply| Command::FinishTurn {
            agent_id,
            turn_id,
            completed: Box::new(completed),
            reply,
        })
        .await
    }

    pub async fn append_event(
        &self,
        agent_id: String,
        turn_id: Option<String>,
        payload: AgentEventPayload,
    ) -> Result<AgentEvent, SessionError> {
        self.call(|reply| Command::AppendEvent {
            agent_id,
            turn_id,
            payload,
            reply,
        })
        .await
    }

    pub async fn events_after(
        &self,
        agent_id: String,
        after_event_id: u64,
    ) -> Result<Vec<AgentEvent>, SessionError> {
        self.call(|reply| Command::EventsAfter {
            agent_id,
            after_event_id,
            reply,
        })
        .await
    }

    pub async fn fork(
        &self,
        source_agent_id: String,
        target_agent_id: String,
        turn_id: Option<String>,
    ) -> Result<ForkSource, SessionError> {
        self.call(|reply| Command::Fork {
            source_agent_id,
            target_agent_id,
            turn_id,
            reply,
        })
        .await
    }

    pub async fn delete(&self, agent_id: String) -> Result<(), SessionError> {
        self.call(|reply| Command::Delete { agent_id, reply }).await
    }

    async fn call<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<T, SessionError>>) -> Command,
    ) -> Result<T, SessionError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(command(reply))
            .await
            .map_err(|_| SessionError::Stopped)?;
        response.await.map_err(|_| SessionError::Stopped)?
    }
}

enum Command {
    EnsureAgent {
        agent_id: String,
        reply: Reply<()>,
    },
    Load {
        agent_id: String,
        reply: Reply<StoredSession>,
    },
    FindRequest {
        agent_id: String,
        key: String,
        reply: Reply<Option<TurnActionResponse>>,
    },
    RecordTurn {
        agent_id: String,
        turn: Box<NewTurn>,
        reply: Reply<AgentEvent>,
    },
    RecordSteer {
        agent_id: String,
        turn_id: String,
        content: Vec<ContentBlock>,
        response: TurnActionResponse,
        idempotency_key: Option<String>,
        reply: Reply<()>,
    },
    MarkStarted {
        agent_id: String,
        turn_id: String,
        reply: Reply<AgentEvent>,
    },
    RequestCancel {
        agent_id: String,
        turn_id: String,
        reply: Reply<AgentEvent>,
    },
    FinishTurn {
        agent_id: String,
        turn_id: String,
        completed: Box<CompletedTurn>,
        reply: Reply<AgentEvent>,
    },
    AppendEvent {
        agent_id: String,
        turn_id: Option<String>,
        payload: AgentEventPayload,
        reply: Reply<AgentEvent>,
    },
    EventsAfter {
        agent_id: String,
        after_event_id: u64,
        reply: Reply<Vec<AgentEvent>>,
    },
    Fork {
        source_agent_id: String,
        target_agent_id: String,
        turn_id: Option<String>,
        reply: Reply<ForkSource>,
    },
    Delete {
        agent_id: String,
        reply: Reply<()>,
    },
}

type Reply<T> = oneshot::Sender<Result<T, SessionError>>;

impl Command {
    fn execute(self, connection: &Connection) {
        match self {
            Self::EnsureAgent { agent_id, reply } => {
                drop(reply.send(ensure_agent(connection, &agent_id)));
            }
            Self::Load { agent_id, reply } => {
                drop(reply.send(load(connection, &agent_id)));
            }
            Self::FindRequest {
                agent_id,
                key,
                reply,
            } => {
                drop(reply.send(find_request(connection, &agent_id, &key)));
            }
            Self::RecordTurn {
                agent_id,
                turn,
                reply,
            } => {
                drop(reply.send(record_turn(connection, &agent_id, *turn)));
            }
            Self::RecordSteer {
                agent_id,
                turn_id,
                content,
                response,
                idempotency_key,
                reply,
            } => {
                drop(reply.send(record_steer(
                    connection,
                    &agent_id,
                    &turn_id,
                    &content,
                    &response,
                    idempotency_key.as_deref(),
                )));
            }
            Self::MarkStarted {
                agent_id,
                turn_id,
                reply,
            } => {
                drop(reply.send(mark_started(connection, &agent_id, &turn_id)));
            }
            Self::RequestCancel {
                agent_id,
                turn_id,
                reply,
            } => {
                drop(reply.send(request_cancel(connection, &agent_id, &turn_id)));
            }
            Self::FinishTurn {
                agent_id,
                turn_id,
                completed,
                reply,
            } => {
                drop(reply.send(finish_turn(connection, &agent_id, &turn_id, *completed)));
            }
            Self::AppendEvent {
                agent_id,
                turn_id,
                payload,
                reply,
            } => {
                drop(reply.send(append_event(
                    connection,
                    &agent_id,
                    turn_id.as_deref(),
                    payload,
                )));
            }
            Self::EventsAfter {
                agent_id,
                after_event_id,
                reply,
            } => {
                drop(reply.send(events_after(connection, &agent_id, after_event_id)));
            }
            Self::Fork {
                source_agent_id,
                target_agent_id,
                turn_id,
                reply,
            } => {
                drop(reply.send(fork(
                    connection,
                    &source_agent_id,
                    &target_agent_id,
                    turn_id.as_deref(),
                )));
            }
            Self::Delete { agent_id, reply } => {
                drop(reply.send(delete(connection, &agent_id)));
            }
        }
    }
}

fn configure(connection: &Connection) -> Result<(), SessionError> {
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.execute_batch(
        r"
        CREATE TABLE IF NOT EXISTS session_agents (
            id TEXT PRIMARY KEY,
            created_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS turns (
            agent_id TEXT NOT NULL REFERENCES session_agents(id) ON DELETE CASCADE,
            id TEXT NOT NULL,
            ordinal INTEGER NOT NULL,
            delivery TEXT NOT NULL,
            state TEXT NOT NULL,
            output_json TEXT NOT NULL DEFAULT '[]',
            error TEXT,
            cancel_requested INTEGER NOT NULL DEFAULT 0,
            attempt INTEGER NOT NULL DEFAULT 0,
            checkpoint_json TEXT,
            completion_event_id INTEGER,
            created_at TEXT NOT NULL,
            completed_at TEXT,
            PRIMARY KEY(agent_id, id),
            UNIQUE(agent_id, ordinal)
        );

        CREATE TABLE IF NOT EXISTS turn_inputs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            agent_id TEXT NOT NULL,
            turn_id TEXT NOT NULL,
            ordinal INTEGER NOT NULL,
            content_json TEXT NOT NULL,
            created_at TEXT NOT NULL,
            FOREIGN KEY(agent_id, turn_id) REFERENCES turns(agent_id, id) ON DELETE CASCADE,
            UNIQUE(agent_id, turn_id, ordinal)
        );

        CREATE TABLE IF NOT EXISTS turn_requests (
            agent_id TEXT NOT NULL REFERENCES session_agents(id) ON DELETE CASCADE,
            idempotency_key TEXT NOT NULL,
            action TEXT NOT NULL,
            turn_id TEXT NOT NULL,
            state TEXT NOT NULL,
            created_at TEXT NOT NULL,
            PRIMARY KEY(agent_id, idempotency_key)
        );

        CREATE TABLE IF NOT EXISTS session_events (
            agent_id TEXT NOT NULL REFERENCES session_agents(id) ON DELETE CASCADE,
            id INTEGER NOT NULL,
            turn_id TEXT,
            payload_json TEXT NOT NULL,
            created_at TEXT NOT NULL,
            PRIMARY KEY(agent_id, id)
        );

        CREATE TABLE IF NOT EXISTS session_forks (
            agent_id TEXT PRIMARY KEY REFERENCES session_agents(id) ON DELETE CASCADE,
            source_agent_id TEXT NOT NULL,
            source_turn_id TEXT NOT NULL,
            created_at TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS turns_agent_state
            ON turns(agent_id, state, ordinal);
        CREATE INDEX IF NOT EXISTS events_agent_id
            ON session_events(agent_id, id);
        ",
    )?;
    Ok(())
}

fn ensure_agent(connection: &Connection, agent_id: &str) -> Result<(), SessionError> {
    connection.execute(
        "INSERT OR IGNORE INTO session_agents (id, created_at) VALUES (?1, ?2)",
        params![agent_id, now()],
    )?;
    Ok(())
}

fn load(connection: &Connection, agent_id: &str) -> Result<StoredSession, SessionError> {
    ensure_agent(connection, agent_id)?;
    let transaction = connection.unchecked_transaction()?;
    let interrupted = {
        let mut statement = transaction.prepare(
            "SELECT id FROM turns
             WHERE agent_id = ?1 AND state = 'running'
             ORDER BY ordinal",
        )?;
        statement
            .query_map(params![agent_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    for turn_id in interrupted {
        transaction.execute(
            "UPDATE turns SET state = 'queued' WHERE agent_id = ?1 AND id = ?2",
            params![agent_id, turn_id],
        )?;
        append_event_tx(
            &transaction,
            agent_id,
            Some(&turn_id),
            AgentEventPayload::TurnInterrupted { retrying: true },
        )?;
    }
    transaction.commit()?;

    let mut statement = connection.prepare(
        "SELECT id, delivery, state, output_json, error, cancel_requested,
                checkpoint_json, created_at, completed_at
         FROM turns WHERE agent_id = ?1 ORDER BY ordinal",
    )?;
    let turns = statement
        .query_map(params![agent_id], |row| {
            let turn_id: String = row.get(0)?;
            let state: String = row.get(2)?;
            let output_json: String = row.get(3)?;
            let checkpoint_json: Option<String> = row.get(6)?;
            let created_at: String = row.get(7)?;
            let completed_at: Option<String> = row.get(8)?;
            Ok(StoredTurn {
                view: TurnView {
                    turn_id: turn_id.clone(),
                    agent_id: agent_id.to_owned(),
                    state: parse_status(&state)?,
                    output: json_from_sql(&output_json)?,
                    error: row.get(4)?,
                    created_at: parse_time(&created_at)?,
                    completed_at: completed_at.as_deref().map(parse_time).transpose()?,
                },
                inputs: load_inputs(connection, agent_id, &turn_id)?,
                cancel_requested: row.get(5)?,
                snapshot: checkpoint_json.as_deref().map(json_from_sql).transpose()?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut requests_statement = connection.prepare(
        "SELECT idempotency_key, action, turn_id, state
         FROM turn_requests WHERE agent_id = ?1",
    )?;
    let requests = requests_statement
        .query_map(params![agent_id], |row| {
            let key: String = row.get(0)?;
            let action: String = row.get(1)?;
            let state: String = row.get(3)?;
            Ok((
                key,
                TurnActionResponse {
                    action: parse_action(&action)?,
                    turn_id: row.get(2)?,
                    state: parse_status(&state)?,
                },
            ))
        })?
        .collect::<Result<HashMap<_, _>, _>>()?;
    Ok(StoredSession { turns, requests })
}

fn load_inputs(
    connection: &Connection,
    agent_id: &str,
    turn_id: &str,
) -> rusqlite::Result<Vec<Vec<ContentBlock>>> {
    let mut statement = connection.prepare(
        "SELECT content_json FROM turn_inputs
         WHERE agent_id = ?1 AND turn_id = ?2 ORDER BY ordinal",
    )?;
    statement
        .query_map(params![agent_id, turn_id], |row| {
            let encoded: String = row.get(0)?;
            json_from_sql(&encoded)
        })?
        .collect()
}

fn find_request(
    connection: &Connection,
    agent_id: &str,
    key: &str,
) -> Result<Option<TurnActionResponse>, SessionError> {
    connection
        .query_row(
            "SELECT action, turn_id, state FROM turn_requests
             WHERE agent_id = ?1 AND idempotency_key = ?2",
            params![agent_id, key],
            |row| {
                let action: String = row.get(0)?;
                let state: String = row.get(2)?;
                Ok(TurnActionResponse {
                    action: parse_action(&action)?,
                    turn_id: row.get(1)?,
                    state: parse_status(&state)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

fn record_turn(
    connection: &Connection,
    agent_id: &str,
    turn: NewTurn,
) -> Result<AgentEvent, SessionError> {
    let transaction = connection.unchecked_transaction()?;
    ensure_agent_tx(&transaction, agent_id)?;
    let ordinal: i64 = transaction.query_row(
        "SELECT COALESCE(MAX(ordinal), 0) + 1 FROM turns WHERE agent_id = ?1",
        params![agent_id],
        |row| row.get(0),
    )?;
    transaction.execute(
        "INSERT INTO turns
         (agent_id, id, ordinal, delivery, state, output_json, error,
          cancel_requested, attempt, created_at, completed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?9, ?10)",
        params![
            agent_id,
            turn.view.turn_id,
            ordinal,
            delivery_name(turn.delivery),
            status_name(turn.view.state),
            serde_json::to_string(&turn.view.output)?,
            turn.view.error,
            i64::from(turn.view.state == TurnStatus::Running),
            turn.view.created_at.to_rfc3339(),
            turn.view.completed_at.map(|time| time.to_rfc3339()),
        ],
    )?;
    insert_input(&transaction, agent_id, &turn.view.turn_id, 1, &turn.content)?;
    if let Some(key) = turn.idempotency_key {
        insert_request(&transaction, agent_id, &key, &turn.response)?;
    }
    let event = append_event_tx(&transaction, agent_id, Some(&turn.view.turn_id), turn.event)?;
    transaction.commit()?;
    Ok(event)
}

fn record_steer(
    connection: &Connection,
    agent_id: &str,
    turn_id: &str,
    content: &[ContentBlock],
    response: &TurnActionResponse,
    idempotency_key: Option<&str>,
) -> Result<(), SessionError> {
    let transaction = connection.unchecked_transaction()?;
    let ordinal: i64 = transaction.query_row(
        "SELECT COALESCE(MAX(ordinal), 0) + 1 FROM turn_inputs
         WHERE agent_id = ?1 AND turn_id = ?2",
        params![agent_id, turn_id],
        |row| row.get(0),
    )?;
    insert_input(&transaction, agent_id, turn_id, ordinal, content)?;
    if let Some(key) = idempotency_key {
        insert_request(&transaction, agent_id, key, response)?;
    }
    transaction.commit()?;
    Ok(())
}

fn mark_started(
    connection: &Connection,
    agent_id: &str,
    turn_id: &str,
) -> Result<AgentEvent, SessionError> {
    let transaction = connection.unchecked_transaction()?;
    transaction.execute(
        "UPDATE turns SET state = 'running', attempt = attempt + 1
         WHERE agent_id = ?1 AND id = ?2",
        params![agent_id, turn_id],
    )?;
    let event = append_event_tx(
        &transaction,
        agent_id,
        Some(turn_id),
        AgentEventPayload::TurnStarted {
            state: TurnStatus::Running,
        },
    )?;
    transaction.commit()?;
    Ok(event)
}

fn request_cancel(
    connection: &Connection,
    agent_id: &str,
    turn_id: &str,
) -> Result<AgentEvent, SessionError> {
    let transaction = connection.unchecked_transaction()?;
    transaction.execute(
        "UPDATE turns SET cancel_requested = 1 WHERE agent_id = ?1 AND id = ?2",
        params![agent_id, turn_id],
    )?;
    let event = append_event_tx(
        &transaction,
        agent_id,
        Some(turn_id),
        AgentEventPayload::TurnCancelRequested,
    )?;
    transaction.commit()?;
    Ok(event)
}

fn finish_turn(
    connection: &Connection,
    agent_id: &str,
    turn_id: &str,
    completed: CompletedTurn,
) -> Result<AgentEvent, SessionError> {
    let transaction = connection.unchecked_transaction()?;
    let event = append_event_tx(&transaction, agent_id, Some(turn_id), completed.event)?;
    transaction.execute(
        "UPDATE turns
         SET state = ?3, output_json = ?4, error = ?5, completed_at = ?6,
             checkpoint_json = ?7, completion_event_id = ?8
         WHERE agent_id = ?1 AND id = ?2",
        params![
            agent_id,
            turn_id,
            status_name(completed.status),
            serde_json::to_string(&completed.output)?,
            completed.error,
            completed.completed_at.to_rfc3339(),
            completed
                .snapshot
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            event.id,
        ],
    )?;
    transaction.commit()?;
    Ok(event)
}

fn append_event(
    connection: &Connection,
    agent_id: &str,
    turn_id: Option<&str>,
    payload: AgentEventPayload,
) -> Result<AgentEvent, SessionError> {
    let transaction = connection.unchecked_transaction()?;
    let event = append_event_tx(&transaction, agent_id, turn_id, payload)?;
    transaction.commit()?;
    Ok(event)
}

fn append_event_tx(
    transaction: &Transaction<'_>,
    agent_id: &str,
    turn_id: Option<&str>,
    payload: AgentEventPayload,
) -> Result<AgentEvent, SessionError> {
    let id: u64 = transaction.query_row(
        "SELECT COALESCE(MAX(id), 0) + 1 FROM session_events WHERE agent_id = ?1",
        params![agent_id],
        |row| row.get(0),
    )?;
    let created_at = Utc::now();
    transaction.execute(
        "INSERT INTO session_events
         (agent_id, id, turn_id, payload_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            agent_id,
            id,
            turn_id,
            serde_json::to_string(&payload)?,
            created_at.to_rfc3339(),
        ],
    )?;
    Ok(AgentEvent {
        id,
        agent_id: agent_id.to_owned(),
        turn_id: turn_id.map(str::to_owned),
        payload,
        created_at,
    })
}

fn events_after(
    connection: &Connection,
    agent_id: &str,
    after_event_id: u64,
) -> Result<Vec<AgentEvent>, SessionError> {
    let mut statement = connection.prepare(
        "SELECT id, turn_id, payload_json, created_at
         FROM session_events WHERE agent_id = ?1 AND id > ?2 ORDER BY id",
    )?;
    statement
        .query_map(params![agent_id, after_event_id], |row| {
            let payload_json: String = row.get(2)?;
            let created_at: String = row.get(3)?;
            Ok(AgentEvent {
                id: row.get(0)?,
                agent_id: agent_id.to_owned(),
                turn_id: row.get(1)?,
                payload: json_from_sql(&payload_json)?,
                created_at: parse_time(&created_at)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn fork(
    connection: &Connection,
    source_agent_id: &str,
    target_agent_id: &str,
    selected_turn_id: Option<&str>,
) -> Result<ForkSource, SessionError> {
    let transaction = connection.unchecked_transaction()?;
    let selected = match selected_turn_id {
        Some(turn_id) => transaction
            .query_row(
                "SELECT id, ordinal, completion_event_id FROM turns
                 WHERE agent_id = ?1 AND id = ?2 AND state = 'completed'",
                params![source_agent_id, turn_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, u64>(2)?,
                    ))
                },
            )
            .optional()?,
        None => transaction
            .query_row(
                "SELECT id, ordinal, completion_event_id FROM turns
                 WHERE agent_id = ?1 AND state = 'completed'
                 ORDER BY ordinal DESC LIMIT 1",
                params![source_agent_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, u64>(2)?,
                    ))
                },
            )
            .optional()?,
    }
    .ok_or(SessionError::ForkBoundaryNotFound)?;
    ensure_agent_tx(&transaction, target_agent_id)?;
    transaction.execute(
        "INSERT INTO turns
         (agent_id, id, ordinal, delivery, state, output_json, error,
          cancel_requested, attempt, checkpoint_json, completion_event_id,
          created_at, completed_at)
         SELECT ?1, id, ordinal, delivery, state, output_json, error,
                cancel_requested, attempt, checkpoint_json, completion_event_id,
                created_at, completed_at
         FROM turns WHERE agent_id = ?2 AND ordinal <= ?3",
        params![target_agent_id, source_agent_id, selected.1],
    )?;
    transaction.execute(
        "INSERT INTO turn_inputs
         (agent_id, turn_id, ordinal, content_json, created_at)
         SELECT ?1, turn_id, ordinal, content_json, created_at
         FROM turn_inputs
         WHERE agent_id = ?2
           AND turn_id IN (
               SELECT id FROM turns WHERE agent_id = ?2 AND ordinal <= ?3
           )",
        params![target_agent_id, source_agent_id, selected.1],
    )?;
    transaction.execute(
        "INSERT INTO session_events
         (agent_id, id, turn_id, payload_json, created_at)
         SELECT ?1, id, turn_id, payload_json, created_at
         FROM session_events WHERE agent_id = ?2 AND id <= ?3",
        params![target_agent_id, source_agent_id, selected.2],
    )?;
    transaction.execute(
        "INSERT INTO session_forks
         (agent_id, source_agent_id, source_turn_id, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![target_agent_id, source_agent_id, selected.0, now()],
    )?;
    transaction.commit()?;
    Ok(ForkSource {
        agent_id: source_agent_id.to_owned(),
        turn_id: Some(selected.0),
    })
}

fn delete(connection: &Connection, agent_id: &str) -> Result<(), SessionError> {
    connection.execute(
        "DELETE FROM session_agents WHERE id = ?1",
        params![agent_id],
    )?;
    Ok(())
}

fn ensure_agent_tx(transaction: &Transaction<'_>, agent_id: &str) -> Result<(), SessionError> {
    transaction.execute(
        "INSERT OR IGNORE INTO session_agents (id, created_at) VALUES (?1, ?2)",
        params![agent_id, now()],
    )?;
    Ok(())
}

fn insert_input(
    transaction: &Transaction<'_>,
    agent_id: &str,
    turn_id: &str,
    ordinal: i64,
    content: &[ContentBlock],
) -> Result<(), SessionError> {
    transaction.execute(
        "INSERT INTO turn_inputs
         (agent_id, turn_id, ordinal, content_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            agent_id,
            turn_id,
            ordinal,
            serde_json::to_string(content)?,
            now(),
        ],
    )?;
    Ok(())
}

fn insert_request(
    transaction: &Transaction<'_>,
    agent_id: &str,
    key: &str,
    response: &TurnActionResponse,
) -> Result<(), SessionError> {
    transaction.execute(
        "INSERT INTO turn_requests
         (agent_id, idempotency_key, action, turn_id, state, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            agent_id,
            key,
            action_name(response.action),
            response.turn_id,
            status_name(response.state),
            now(),
        ],
    )?;
    Ok(())
}

const fn status_name(status: TurnStatus) -> &'static str {
    match status {
        TurnStatus::Queued => "queued",
        TurnStatus::Running => "running",
        TurnStatus::Completed => "completed",
        TurnStatus::Failed => "failed",
        TurnStatus::Cancelled => "cancelled",
    }
}

fn parse_status(value: &str) -> rusqlite::Result<TurnStatus> {
    match value {
        "queued" => Ok(TurnStatus::Queued),
        "running" => Ok(TurnStatus::Running),
        "completed" => Ok(TurnStatus::Completed),
        "failed" => Ok(TurnStatus::Failed),
        "cancelled" => Ok(TurnStatus::Cancelled),
        _ => Err(invalid_sql("turn status")),
    }
}

const fn delivery_name(delivery: TurnDelivery) -> &'static str {
    match delivery {
        TurnDelivery::Steer => "steer",
        TurnDelivery::Enqueue => "enqueue",
    }
}

const fn action_name(action: TurnAction) -> &'static str {
    match action {
        TurnAction::Started => "started",
        TurnAction::Steered => "steered",
        TurnAction::Queued => "queued",
    }
}

fn parse_action(value: &str) -> rusqlite::Result<TurnAction> {
    match value {
        "started" => Ok(TurnAction::Started),
        "steered" => Ok(TurnAction::Steered),
        "queued" => Ok(TurnAction::Queued),
        _ => Err(invalid_sql("turn action")),
    }
}

fn parse_time(value: &str) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
}

fn json_from_sql<T: serde::de::DeserializeOwned>(value: &str) -> rusqlite::Result<T> {
    serde_json::from_str(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

fn invalid_sql(name: &'static str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        format!("invalid {name}").into(),
    )
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

#[derive(Debug, Error)]
pub(crate) enum SessionError {
    #[error("session SQLite actor stopped")]
    Stopped,
    #[error("completed fork boundary was not found")]
    ForkBoundaryNotFound,
    #[error("session SQLite failed")]
    Database(#[from] rusqlite::Error),
    #[error("session JSON encoding failed")]
    Json(#[from] serde_json::Error),
    #[error("session filesystem setup failed")]
    Io(#[from] std::io::Error),
}

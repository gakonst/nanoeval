use nanocodex::AgentEvent;
use serde::Serialize;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::{EvalResult, VerifierResult};

/// One event from a possibly concurrent Nanoeval attempt.
#[derive(Clone, Debug, Serialize)]
pub struct EvalEvent {
    pub attempt_id: Uuid,
    pub task_name: String,
    #[serde(flatten)]
    pub kind: EvalEventKind,
}

/// Agent and verifier activity exposed independently from [`EvalResult`].
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum EvalEventKind {
    Agent(AgentEvent),
    VerifierStarted,
    VerifierOutput { stdout: String, stderr: String },
    VerifierCompleted(VerifierResult),
    Completed(Box<EvalResult>),
}

/// Receiving half of a reusable Nanoeval instance's multiplexed event stream.
pub struct NanoevalEvents {
    receiver: mpsc::UnboundedReceiver<EvalEvent>,
}

impl NanoevalEvents {
    pub(crate) fn new(receiver: mpsc::UnboundedReceiver<EvalEvent>) -> Self {
        Self { receiver }
    }

    /// Receives the next event, or `None` after the evaluator is dropped and
    /// all active attempts finish.
    pub async fn recv(&mut self) -> Option<EvalEvent> {
        self.receiver.recv().await
    }
}

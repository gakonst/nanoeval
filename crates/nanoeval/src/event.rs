use std::{path::PathBuf, sync::Arc};

use nanocodex::AgentEvent;
use serde::Serialize;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::{EvalFailure, EvalResult, VerifierResult};

/// One event from a possibly concurrent Nanoeval attempt.
#[derive(Clone, Debug, Serialize)]
pub struct EvalEvent {
    pub run_id: Uuid,
    pub attempt_id: Uuid,
    pub task_name: String,
    pub trial_name: String,
    pub sequence: u64,
    #[serde(flatten)]
    pub kind: EvalEventKind,
}

/// Agent and verifier activity exposed independently from [`EvalResult`].
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum EvalEventKind {
    AttemptStarted { prompt: String, workspace: PathBuf },
    Agent(AgentEvent),
    VerifierStarted,
    VerifierOutput { stdout: String, stderr: String },
    VerifierCompleted(VerifierResult),
    Completed(Box<EvalResult>),
    Failed(Box<EvalFailure>),
}

/// Cloneable source of independent subscriptions to one evaluation job.
#[derive(Clone)]
pub struct NanoevalEvents {
    sender: broadcast::Sender<Arc<EvalEvent>>,
}

/// One independent, ordered subscription to an evaluation job.
pub struct NanoevalEventStream {
    receiver: broadcast::Receiver<Arc<EvalEvent>>,
}

#[derive(Debug, thiserror::Error)]
pub enum EvalEventStreamError {
    #[error("event subscriber fell behind and missed {missed} events")]
    Lagged { missed: u64 },
}

impl NanoevalEvents {
    pub(crate) fn new(sender: broadcast::Sender<Arc<EvalEvent>>) -> Self {
        Self { sender }
    }

    /// Subscribes before attempts start. Each subscription receives the same
    /// subsequent events independently.
    #[must_use]
    pub fn subscribe(&self) -> NanoevalEventStream {
        NanoevalEventStream {
            receiver: self.sender.subscribe(),
        }
    }
}

impl NanoevalEventStream {
    /// Receives the next event, `None` after the run closes, or an explicit
    /// lag error rather than silently skipping events.
    ///
    /// # Errors
    ///
    /// Returns [`EvalEventStreamError::Lagged`] when this subscriber did not
    /// keep up with the bounded event journal.
    pub async fn recv(&mut self) -> Result<Option<Arc<EvalEvent>>, EvalEventStreamError> {
        match self.receiver.recv().await {
            Ok(event) => Ok(Some(event)),
            Err(broadcast::error::RecvError::Closed) => Ok(None),
            Err(broadcast::error::RecvError::Lagged(missed)) => {
                Err(EvalEventStreamError::Lagged { missed })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::broadcast;
    use uuid::Uuid;

    use super::{EvalEvent, EvalEventKind, EvalEventStreamError, NanoevalEvents};

    #[tokio::test]
    async fn subscriptions_receive_the_same_event_independently() {
        let (sender, _) = broadcast::channel(4);
        let events = NanoevalEvents::new(sender.clone());
        let mut first = events.subscribe();
        let mut second = events.subscribe();
        let event = Arc::new(event(1));

        sender.send(Arc::clone(&event)).unwrap();

        let first = first.recv().await.unwrap().unwrap();
        let second = second.recv().await.unwrap().unwrap();
        assert!(Arc::ptr_eq(&first, &event));
        assert!(Arc::ptr_eq(&second, &event));
    }

    #[tokio::test]
    async fn lag_is_reported_instead_of_silently_skipping_events() {
        let (sender, _) = broadcast::channel(1);
        let events = NanoevalEvents::new(sender.clone());
        let mut subscriber = events.subscribe();

        sender.send(Arc::new(event(1))).unwrap();
        sender.send(Arc::new(event(2))).unwrap();

        assert!(matches!(
            subscriber.recv().await,
            Err(EvalEventStreamError::Lagged { missed: 1 })
        ));
        assert_eq!(subscriber.recv().await.unwrap().unwrap().sequence, 2);
    }

    fn event(sequence: u64) -> EvalEvent {
        EvalEvent {
            run_id: Uuid::nil(),
            attempt_id: Uuid::nil(),
            task_name: "task".to_owned(),
            trial_name: "task__attempt".to_owned(),
            sequence,
            kind: EvalEventKind::VerifierStarted,
        }
    }
}

use async_trait::async_trait;
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Successful MPP proof returned to the client with a paid mutation.
#[derive(Clone, Debug)]
pub struct PaymentReceipt {
    pub header_value: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PaymentManagementResponse {
    pub status: PaymentManagementStatus,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentManagementStatus {
    Ok,
}

/// A payment request is either billable application traffic, a protocol
/// management message, or missing a credential and therefore challenged.
#[derive(Clone, Debug)]
pub enum PaymentOutcome {
    Authorized(PaymentReceipt),
    Challenge {
        www_authenticate: String,
    },
    Management {
        body: PaymentManagementResponse,
        receipt: PaymentReceipt,
    },
}

#[async_trait]
pub trait PaymentGate: Send + Sync {
    async fn authorize(&self, headers: &HeaderMap) -> Result<PaymentOutcome, PaymentError>;
}

/// Development/test gate. Production starts with an MPP gate in the binary.
pub struct FreePaymentGate;

#[async_trait]
impl PaymentGate for FreePaymentGate {
    async fn authorize(&self, _headers: &HeaderMap) -> Result<PaymentOutcome, PaymentError> {
        Ok(PaymentOutcome::Authorized(PaymentReceipt {
            header_value: "free-development-mode".to_owned(),
        }))
    }
}

#[derive(Debug, Error)]
pub enum PaymentError {
    #[error("invalid payment credential")]
    InvalidCredential,
    #[error("payment configuration failed: {0}")]
    Configuration(String),
    #[error("payment verification failed: {0}")]
    Verification(String),
}

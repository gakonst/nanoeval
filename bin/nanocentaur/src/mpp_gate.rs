use std::sync::Arc;

use async_trait::async_trait;
use axum::http::{HeaderMap, header};
use mpp::{
    Address, PaymentCredential, PrivateKeySigner, parse_authorization,
    server::{
        Mpp, SessionChallengeOptions, SessionChannelStore, SessionMethodConfig, TempoChargeMethod,
        TempoConfig, TempoProvider, TempoSessionMethod, tempo, tempo_provider,
    },
};
use nanocentaur::{
    PaymentError, PaymentGate, PaymentManagementResponse, PaymentOutcome, PaymentReceipt,
};

type PaymentHandler = Mpp<TempoChargeMethod<TempoProvider>, TempoSessionMethod<TempoProvider>>;

pub struct MppGateConfig {
    pub rpc_url: String,
    pub currency: String,
    pub recipient: String,
    pub escrow_contract: String,
    pub chain_id: u64,
    pub close_key: String,
    pub challenge_secret: String,
    pub unit_price: u128,
    pub suggested_deposit: Option<String>,
    pub fee_payer: bool,
}

pub struct MppSessionGate {
    payment: PaymentHandler,
    unit_price: u128,
    currency: String,
    recipient: String,
    suggested_deposit: Option<String>,
}

impl MppSessionGate {
    pub fn new(config: MppGateConfig) -> Result<Self, PaymentError> {
        let signer: PrivateKeySigner = config
            .close_key
            .parse()
            .map_err(|error| PaymentError::Configuration(format!("invalid close key: {error}")))?;
        let signer_address = format!("{:#x}", signer.address());
        if !signer_address.eq_ignore_ascii_case(&config.recipient) {
            return Err(PaymentError::Configuration(
                "MPP close key must sign for the configured recipient".to_owned(),
            ));
        }
        let escrow_contract: Address = config.escrow_contract.parse().map_err(|error| {
            PaymentError::Configuration(format!("invalid escrow contract: {error}"))
        })?;
        let store = Arc::new(SessionChannelStore::new());
        let payment = Mpp::create(
            tempo(TempoConfig {
                recipient: &config.recipient,
            })
            .rpc_url(&config.rpc_url)
            .currency(&config.currency)
            .secret_key(&config.challenge_secret)
            .fee_payer(config.fee_payer),
        )
        .map_err(|error| PaymentError::Configuration(error.to_string()))?;
        let provider = tempo_provider(&config.rpc_url)
            .map_err(|error| PaymentError::Configuration(error.to_string()))?;
        let session = TempoSessionMethod::new(
            provider,
            store,
            SessionMethodConfig {
                escrow_contract,
                chain_id: config.chain_id,
                min_voucher_delta: config.unit_price,
            },
        )
        .with_close_signer(signer);
        Ok(Self {
            payment: payment.with_session_method(session),
            unit_price: config.unit_price,
            currency: config.currency,
            recipient: config.recipient,
            suggested_deposit: config.suggested_deposit,
        })
    }
}

#[async_trait]
impl PaymentGate for MppSessionGate {
    async fn authorize(&self, headers: &HeaderMap) -> Result<PaymentOutcome, PaymentError> {
        let Some(credential) = parse_credential(headers)? else {
            let price = self.unit_price.to_string();
            let challenge = self
                .payment
                .session_challenge_with_details(
                    &price,
                    &self.currency,
                    &self.recipient,
                    SessionChallengeOptions {
                        unit_type: Some("agent_turn"),
                        suggested_deposit: self.suggested_deposit.as_deref(),
                        description: Some("one nanocentaur agent turn"),
                        ..Default::default()
                    },
                )
                .map_err(|error| PaymentError::Configuration(error.to_string()))?;
            return Ok(PaymentOutcome::Challenge {
                www_authenticate: challenge
                    .to_header()
                    .map_err(|error| PaymentError::Configuration(error.to_string()))?,
            });
        };

        let verified = self
            .payment
            .verify_session(&credential)
            .await
            .map_err(|error| PaymentError::Verification(error.to_string()))?;
        let receipt = PaymentReceipt {
            header_value: verified
                .receipt
                .to_header()
                .map_err(|error| PaymentError::Verification(error.to_string()))?,
        };
        if let Some(management) = verified.management_response {
            return Ok(PaymentOutcome::Management {
                body: serde_json::from_value::<PaymentManagementResponse>(management)
                    .map_err(|error| PaymentError::Verification(error.to_string()))?,
                receipt,
            });
        }
        Ok(PaymentOutcome::Authorized(receipt))
    }
}

fn parse_credential(headers: &HeaderMap) -> Result<Option<PaymentCredential>, PaymentError> {
    headers
        .get(header::AUTHORIZATION)
        .map(|value| {
            value
                .to_str()
                .map_err(|_| PaymentError::InvalidCredential)
                .and_then(|value| {
                    parse_authorization(value).map_err(|_| PaymentError::InvalidCredential)
                })
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_credential_produces_an_mpp_session_challenge() {
        let gate = MppSessionGate::new(MppGateConfig {
            rpc_url: "http://127.0.0.1:1".to_owned(),
            currency: "0x0000000000000000000000000000000000000001".to_owned(),
            recipient: "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf".to_owned(),
            escrow_contract: "0x0000000000000000000000000000000000000002".to_owned(),
            chain_id: 1,
            close_key: "0x0000000000000000000000000000000000000000000000000000000000000001"
                .to_owned(),
            challenge_secret: "test-only-challenge-secret".to_owned(),
            unit_price: 25,
            suggested_deposit: Some("250".to_owned()),
            fee_payer: false,
        })
        .unwrap();
        let outcome = gate.authorize(&HeaderMap::new()).await.unwrap();
        let PaymentOutcome::Challenge { www_authenticate } = outcome else {
            panic!("missing credential must return a challenge");
        };
        assert!(!www_authenticate.is_empty());
        assert!(www_authenticate.contains("session"));
    }
}

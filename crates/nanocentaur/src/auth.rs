use axum::http::HeaderMap;
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Separate credential for the policy administration surface. Agent API keys
/// are resolved through `SQLite` and are never accepted by `/admin/v1/*`.
pub struct AdminAuthorizer {
    token_digest: [u8; 32],
}

impl AdminAuthorizer {
    /// Constructs the separate administrator credential verifier.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured token is empty.
    pub fn new(token: impl AsRef<str>) -> Result<Self, AuthorizationError> {
        let token = token.as_ref();
        if token.is_empty() {
            return Err(AuthorizationError::InvalidConfiguration(
                "admin token must not be empty",
            ));
        }
        Ok(Self {
            token_digest: Sha256::digest(token.as_bytes()).into(),
        })
    }

    /// Verifies an administrator bearer token.
    ///
    /// # Errors
    ///
    /// Returns `Unauthenticated` when the bearer token is absent or invalid.
    pub fn authorize(&self, headers: &HeaderMap) -> Result<(), AuthorizationError> {
        let token = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .filter(|value| !value.is_empty())
            .ok_or(AuthorizationError::Unauthenticated)?;
        let candidate: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        if constant_time_eq(&candidate, &self.token_digest) {
            Ok(())
        } else {
            Err(AuthorizationError::Unauthenticated)
        }
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum AuthorizationError {
    #[error("authentication required")]
    Unauthenticated,
    #[error("invalid authorization configuration: {0}")]
    InvalidConfiguration(&'static str),
}

fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn admin_auth_only_accepts_its_bearer_token() {
        let auth = AdminAuthorizer::new("admin-secret").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer admin-secret"),
        );
        assert!(auth.authorize(&headers).is_ok());

        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer agent-key"),
        );
        assert_eq!(
            auth.authorize(&headers).unwrap_err(),
            AuthorizationError::Unauthenticated
        );
    }
}

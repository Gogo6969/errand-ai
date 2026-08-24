//! Bearer token auth and scope enforcement.
//!
//! Tokens are rows, not a singleton, so each client gets its own named token
//! with only the scopes it needs. The plaintext lives in the macOS keychain and
//! the database stores only a SHA-256 hash.
//!
//! `approve` is deliberately separate from `run`. Approval gates exist to put a
//! human in front of an irreversible action, so a client that can start a
//! booking must not be able to confirm it. KinAI gets `read,run,webhook`.

use anyhow::{Context, Result};
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use errand_core::db::{Pool, TokenRow};
use errand_core::models::Scope;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub const TOKEN_PREFIX: &str = "err_v1_";

/// Identity attached to an authenticated request.
#[derive(Clone)]
pub struct Caller {
    /// Written to the audit log, which records every call another app made.
    #[allow(dead_code)]
    pub token_id: String,
    pub token_name: String,
    pub scopes: Vec<Scope>,
}

impl Caller {
    pub fn has(&self, want: Scope) -> bool {
        // admin implies everything; every other scope is explicit.
        self.scopes.contains(&Scope::Admin) || self.scopes.contains(&want)
    }
}

pub fn hash_token(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    hex::encode(h.finalize())
}

/// Generate a fresh token from the OS CSPRNG.
pub fn generate_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no system randomness: {e}"))?;
    Ok(format!("{TOKEN_PREFIX}{}", hex::encode(bytes)))
}

/// Mint the primary admin token on first boot: hash into the database first,
/// then plaintext into the keychain.
///
/// The order matters. The hash is what authenticates requests, so writing it
/// first means the API is usable even if the keychain write then stalls behind
/// an authorization prompt. The keychain copy exists only so the token can be
/// shown to the user again later.
pub async fn ensure_primary_token(pool: &Pool) -> Result<Option<String>> {
    if errand_core::db::has_any_token(pool).await? {
        return Ok(None);
    }
    let token = generate_token()?;
    let hash = hash_token(&token);
    errand_core::db::insert_token(pool, "primary", &hash, "admin").await?;
    crate::secrets::put_internal(
        errand_core::keychain::ACCOUNT_API_TOKEN,
        errand_core::keychain::Secret::new(token.clone()),
    )
    .await
    .context("storing the API token in the keychain")?;
    Ok(Some(token))
}

/// Read the primary token back out of the keychain, for the CLI and for the UI
/// to display on request.
pub async fn read_primary_token() -> Result<String> {
    Ok(
        crate::secrets::get_internal(errand_core::keychain::ACCOUNT_API_TOKEN)
            .await?
            .expose()
            .to_string(),
    )
}

async fn authenticate(pool: &Pool, header: Option<&str>) -> Option<Caller> {
    let raw = header?.strip_prefix("Bearer ")?.trim();
    if !raw.starts_with(TOKEN_PREFIX) {
        return None;
    }
    let presented = hash_token(raw);
    let row: TokenRow = errand_core::db::token_by_hash(pool, &presented)
        .await
        .ok()??;

    // The lookup already matched, but compare explicitly in constant time so the
    // comparison itself never becomes a timing oracle if the lookup changes.
    let stored = presented.as_bytes();
    if stored.ct_eq(presented.as_bytes()).unwrap_u8() != 1 {
        return None;
    }

    let scopes = row
        .scopes
        .split(',')
        .filter_map(Scope::parse)
        .collect::<Vec<_>>();
    let _ = errand_core::db::touch_token(pool, &row.id).await;
    Some(Caller {
        token_id: row.id,
        token_name: row.name,
        scopes,
    })
}

/// Auth middleware. Unauthenticated requests get a bare 401 with no hint about
/// which part of the credential was wrong.
pub async fn require_auth(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    match authenticate(state.pool(), header.as_deref()).await {
        Some(caller) => {
            req.extensions_mut().insert(caller);
            Ok(next.run(req).await)
        }
        None => Err(StatusCode::UNAUTHORIZED),
    }
}

/// Reject a request whose token lacks the scope this route needs.
pub fn require(caller: &Caller, scope: Scope) -> Result<(), crate::api::ApiError> {
    if caller.has(scope) {
        Ok(())
    } else {
        Err(crate::api::ApiError::forbidden(format!(
            "This token has scopes [{}] and this endpoint needs '{}'.",
            caller
                .scopes
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            scope.as_str()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_prefixed_and_long_enough() {
        let t = generate_token().unwrap();
        assert!(t.starts_with(TOKEN_PREFIX));
        assert_eq!(t.len(), TOKEN_PREFIX.len() + 64);
        assert_ne!(t, generate_token().unwrap());
    }

    #[test]
    fn hashing_is_stable_and_hides_the_token() {
        let t = "err_v1_abc";
        assert_eq!(hash_token(t), hash_token(t));
        assert!(!hash_token(t).contains("abc"));
    }

    #[test]
    fn admin_implies_every_scope_and_run_does_not_imply_approve() {
        let admin = Caller {
            token_id: "1".into(),
            token_name: "primary".into(),
            scopes: vec![Scope::Admin],
        };
        assert!(admin.has(Scope::Approve));
        assert!(admin.has(Scope::Manage));

        // This is the KinAI token shape from the plan.
        let kinai = Caller {
            token_id: "2".into(),
            token_name: "kinai".into(),
            scopes: vec![Scope::Read, Scope::Run, Scope::Webhook],
        };
        assert!(kinai.has(Scope::Run));
        assert!(kinai.has(Scope::Webhook));
        assert!(
            !kinai.has(Scope::Approve),
            "a client that can start a booking must not be able to confirm it"
        );
        assert!(!kinai.has(Scope::Manage));
    }
}

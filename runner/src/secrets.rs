//! Time-bounded keychain access.
//!
//! A keychain read can block indefinitely. When the item's ACL does not match
//! the calling binary, macOS puts up an authorization prompt, and under launchd
//! there is no window for that prompt to appear in, so the call never returns.
//! A daemon that mints its token before it binds its port will hang forever in
//! exactly that situation, with no log and no way to diagnose it.
//!
//! So every keychain call from the daemon goes through here: off the async
//! runtime onto a blocking thread, with a deadline, and with an error message
//! that names the real cause.

use anyhow::{anyhow, Result};
use errand_core::keychain::{CredStore, Secret};
use std::time::Duration;

/// Long enough for a healthy keychain, short enough that a blocked prompt does
/// not look like a hang.
pub const TIMEOUT: Duration = Duration::from_secs(5);

fn stalled(op: &str) -> anyhow::Error {
    anyhow!(
        "The keychain did not answer within {}s while trying to {op}. \
         That almost always means macOS is showing an authorization prompt for an item \
         whose access list no longer matches this build of Errand. \
         Open Keychain Access, delete the 'com.errandai.app' items, and restart the runner. \
         Rebuilt development binaries are re-signed each time, so their items need clearing \
         after a rebuild.",
        TIMEOUT.as_secs()
    )
}

pub async fn put(service: String, account: String, secret: Secret) -> Result<()> {
    let value = secret.expose().to_string();
    let task = tokio::task::spawn_blocking(move || {
        errand_core::keychain::store().put(&service, &account, &Secret::new(value))
    });
    match tokio::time::timeout(TIMEOUT, task).await {
        Ok(joined) => joined?,
        Err(_) => Err(stalled("save a secret")),
    }
}

pub async fn get(service: String, account: String) -> Result<Secret> {
    let task =
        tokio::task::spawn_blocking(move || errand_core::keychain::store().get(&service, &account));
    match tokio::time::timeout(TIMEOUT, task).await {
        Ok(joined) => joined?,
        Err(_) => Err(stalled("read a secret")),
    }
}

pub async fn delete(service: String, account: String) -> Result<()> {
    let task = tokio::task::spawn_blocking(move || {
        errand_core::keychain::store().delete(&service, &account)
    });
    match tokio::time::timeout(TIMEOUT, task).await {
        Ok(joined) => joined?,
        Err(_) => Err(stalled("delete a secret")),
    }
}

pub async fn put_internal(account: &str, secret: Secret) -> Result<()> {
    put(
        errand_core::keychain_service_internal(),
        account.to_string(),
        secret,
    )
    .await
}

pub async fn get_internal(account: &str) -> Result<Secret> {
    get(
        errand_core::keychain_service_internal(),
        account.to_string(),
    )
    .await
}

/// Is the keychain answering at all? Used by health and by doctor. Never
/// returns an error: the point is to report a state, not to fail.
pub async fn probe() -> KeychainState {
    let service = format!("{}.probe", errand_core::keychain_service_internal());
    let account = "healthcheck".to_string();
    let put_res = put(
        service.clone(),
        account.clone(),
        Secret::new("probe".into()),
    )
    .await;
    if let Err(e) = put_res {
        return if e.to_string().contains("did not answer") {
            KeychainState::Blocked
        } else {
            KeychainState::Error
        };
    }
    let ok = get(service.clone(), account.clone())
        .await
        .map(|s| s.expose() == "probe")
        .unwrap_or(false);
    let _ = delete(service, account).await;
    if ok {
        KeychainState::Ok
    } else {
        KeychainState::Error
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeychainState {
    Ok,
    /// An authorization prompt is almost certainly waiting somewhere invisible.
    Blocked,
    Error,
}

impl KeychainState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Blocked => "blocked",
            Self::Error => "error",
        }
    }
}

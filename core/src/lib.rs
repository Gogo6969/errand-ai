//! errand-core: everything the UI shell and the daemon both need.
//!
//! The daemon (`errandd`) is the only process that writes to the database,
//! reads the keychain, or acts on the world. This crate holds the shared
//! vocabulary so the two binaries cannot drift apart.

pub mod db;
pub mod keychain;
pub mod launchd;
pub mod models;
pub mod paths;
pub mod playbook;
pub mod schedule;

/// Bundle identifier of the app. Data directory derives from this.
pub const APP_ID: &str = "com.errandai.app";

/// Code identity of the daemon. Automation consent and keychain ACLs bind
/// here, not to APP_ID, because the daemon is what sends the Apple Event and
/// reads the secret.
pub const RUNNER_ID: &str = "com.errandai.runner";

/// launchd label for the background runner.
pub const LAUNCHD_LABEL: &str = "com.errandai.runner";

/// Keychain service for site credentials. Dev builds append `.dev` so
/// development and production items never collide.
pub const KEYCHAIN_SERVICE: &str = "com.errandai.app.credentials";

/// Keychain service for the app's own secrets (API tokens, bot tokens).
pub const KEYCHAIN_SERVICE_INTERNAL: &str = "com.errandai.app.internal";

/// Default loopback port for the local API.
pub const DEFAULT_API_PORT: u16 = 4477;

/// Schema version the code expects. The daemon migrates; the UI never does.
/// The supervisor compares this against `GET /v1/health` after an update so an
/// old daemon serving a new UI is detected instead of discovered through bugs.
pub const SCHEMA_VERSION: i64 = 1;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// True when this build should use the `.dev` keychain suffix.
pub fn is_dev_build() -> bool {
    cfg!(debug_assertions) || std::env::var("ERRAND_DEV").is_ok()
}

/// Keychain service name for site credentials, dev-aware.
pub fn keychain_service() -> String {
    if is_dev_build() {
        format!("{KEYCHAIN_SERVICE}.dev")
    } else {
        KEYCHAIN_SERVICE.to_string()
    }
}

/// Keychain service name for internal secrets, dev-aware.
pub fn keychain_service_internal() -> String {
    if is_dev_build() {
        format!("{KEYCHAIN_SERVICE_INTERNAL}.dev")
    } else {
        KEYCHAIN_SERVICE_INTERNAL.to_string()
    }
}

/// A time-sortable id, used for every entity in the system.
pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// ISO-8601 UTC, the one timestamp format in the database.
pub fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

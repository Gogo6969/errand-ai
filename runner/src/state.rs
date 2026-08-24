//! Shared daemon state and the event bus.

use anyhow::Result;
use errand_core::db::Pool;
use errand_core::models::Event;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::broadcast;

/// Every live event passes through one broadcast channel. The per-run SSE
/// stream, the global firehose, and (later) the Tauri event bridge are all
/// subscribers to this, which is what keeps the four surfaces from drifting
/// into four vocabularies.
#[derive(Clone)]
pub struct AppState(Arc<Inner>);

pub struct Inner {
    pub pool: Pool,
    pub events: broadcast::Sender<Event>,
    pub started: Instant,
    /// Set when a quiesce is in flight so no new work is accepted while the
    /// updater is swapping the bundle underneath us.
    pub quiescing: std::sync::atomic::AtomicBool,
    /// Reported by health so a blocked keychain is visible rather than being a
    /// mystery hang. 0 unknown, 1 ok, 2 blocked, 3 error.
    pub keychain: std::sync::atomic::AtomicU8,
    /// Port the API is listening on, needed to build each run's MCP URL.
    pub api_port: std::sync::atomic::AtomicU16,
    /// Per-run bearer tokens for the MCP tool server. A token scopes every tool
    /// call to one run, so the agent cannot reach another task's data.
    pub run_tokens: parking_lot::Mutex<std::collections::HashMap<String, String>>,
    /// Outcomes reported by agents through the finish and fail tools.
    pub outcomes: parking_lot::Mutex<std::collections::HashMap<String, crate::mcp::Outcome>>,
    /// The browser each run is driving, if it has opened one.
    pub browsers: tokio::sync::Mutex<
        std::collections::HashMap<String, std::sync::Arc<crate::browser::Browser>>,
    >,
    /// One redactor per run, seeded with every secret that run resolves.
    pub redactors: parking_lot::Mutex<std::collections::HashMap<String, crate::redact::Redactor>>,
}

impl AppState {
    pub fn new(pool: Pool) -> Self {
        let (tx, _rx) = broadcast::channel(1024);
        Self(Arc::new(Inner {
            pool,
            events: tx,
            started: Instant::now(),
            quiescing: std::sync::atomic::AtomicBool::new(false),
            keychain: std::sync::atomic::AtomicU8::new(0),
            api_port: std::sync::atomic::AtomicU16::new(errand_core::DEFAULT_API_PORT),
            run_tokens: parking_lot::Mutex::new(std::collections::HashMap::new()),
            outcomes: parking_lot::Mutex::new(std::collections::HashMap::new()),
            browsers: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            redactors: parking_lot::Mutex::new(std::collections::HashMap::new()),
        }))
    }

    /// The redactor for a run, created on first use. Every secret the run
    /// touches is registered here, and everything leaving the run is scrubbed
    /// through it.
    pub fn redactor(&self, run_id: &str) -> crate::redact::Redactor {
        let mut g = self.0.redactors.lock();
        g.entry(run_id.to_string()).or_default().clone()
    }

    pub async fn browser(&self, run_id: &str) -> Option<std::sync::Arc<crate::browser::Browser>> {
        self.0.browsers.lock().await.get(run_id).cloned()
    }

    pub async fn set_browser(&self, run_id: &str, b: std::sync::Arc<crate::browser::Browser>) {
        self.0.browsers.lock().await.insert(run_id.to_string(), b);
    }

    /// Shut a run's browser down and release everything it held.
    pub async fn close_browser(&self, run_id: &str) {
        let b = self.0.browsers.lock().await.remove(run_id);
        if let Some(b) = b {
            b.close().await;
        }
        let _ = errand_core::db::release_browser_profiles(self.pool(), run_id).await;
        self.0.redactors.lock().remove(run_id);
    }

    pub fn set_api_port(&self, p: u16) {
        self.0
            .api_port
            .store(p, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn api_port(&self) -> u16 {
        self.0.api_port.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Mint the bearer the agent's tool calls must present for this run.
    pub fn mint_run_token(&self, run_id: &str) -> String {
        let token = crate::api::auth::generate_token()
            .unwrap_or_else(|_| errand_core::new_id().replace('-', ""));
        self.0
            .run_tokens
            .lock()
            .insert(run_id.to_string(), token.clone());
        token
    }

    /// Constant-time check of a run's MCP bearer.
    pub fn verify_run_token(&self, run_id: &str, presented: &str) -> bool {
        use subtle::ConstantTimeEq;
        let guard = self.0.run_tokens.lock();
        let Some(expected) = guard.get(run_id) else {
            return false;
        };
        expected.len() == presented.len()
            && expected.as_bytes().ct_eq(presented.as_bytes()).unwrap_u8() == 1
    }

    pub fn clear_run_token(&self, run_id: &str) {
        self.0.run_tokens.lock().remove(run_id);
        self.0.outcomes.lock().remove(run_id);
    }

    pub fn set_outcome(&self, run_id: &str, o: crate::mcp::Outcome) {
        self.0.outcomes.lock().insert(run_id.to_string(), o);
    }

    pub fn take_outcome(&self, run_id: &str) -> Option<crate::mcp::Outcome> {
        self.0.outcomes.lock().remove(run_id)
    }

    /// The signing secret for a webhook, from the keychain.
    ///
    /// In the keychain rather than the database for the same reason as every
    /// other secret: the database is backed up, copied and read by tooling, and
    /// this one lets a receiver believe a callback is genuine.
    pub async fn webhook_secret(&self, webhook_id: &str) -> Option<String> {
        crate::secrets::get_internal(&format!("webhook.{webhook_id}"))
            .await
            .ok()
            .map(|s| s.expose().to_string())
    }

    pub fn set_keychain(&self, s: crate::secrets::KeychainState) {
        let v = match s {
            crate::secrets::KeychainState::Ok => 1,
            crate::secrets::KeychainState::Blocked => 2,
            crate::secrets::KeychainState::Error => 3,
        };
        self.0
            .keychain
            .store(v, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn keychain_str(&self) -> &'static str {
        match self.0.keychain.load(std::sync::atomic::Ordering::SeqCst) {
            1 => "ok",
            2 => "blocked",
            3 => "error",
            _ => "checking",
        }
    }

    pub fn pool(&self) -> &Pool {
        &self.0.pool
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.0.events.subscribe()
    }

    /// Publish an event. A send failure only means nobody is listening yet,
    /// which is normal and not worth logging.
    pub fn emit(&self, ev: Event) {
        let _ = self.0.events.send(ev);
    }

    pub fn uptime_s(&self) -> u64 {
        self.0.started.elapsed().as_secs()
    }

    pub fn is_quiescing(&self) -> bool {
        self.0.quiescing.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn begin_quiesce(&self) {
        self.0
            .quiescing
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub async fn health(&self) -> Result<errand_core::models::Health> {
        let busy = errand_core::db::count_busy_runs(self.pool())
            .await
            .unwrap_or(-1);
        Ok(errand_core::models::Health {
            status: if self.is_quiescing() {
                "quiescing"
            } else {
                "ok"
            }
            .into(),
            version: errand_core::VERSION.into(),
            schema_version: errand_core::SCHEMA_VERSION,
            uptime_s: self.uptime_s(),
            busy_runs: busy,
            db: if busy >= 0 { "ok" } else { "error" }.into(),
            scheduler: "idle".into(),
            keychain: self.keychain_str().into(),
        })
    }
}

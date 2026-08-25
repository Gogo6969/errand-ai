//! A real daemon on a real port, for tests that have to go through the API.
//!
//! The rules worth testing here (a permission, a refusal, a site list being
//! rewritten into the form the browser will compare) all live between the wire
//! and the database. A test that calls a handler directly skips the token and
//! the routing; a test that writes rows skips everything. Either can pass while
//! the product is broken, which this repository has managed twice. So the tests
//! speak HTTP and set their state up through the same calls the app makes.

use errand_core::db::Pool;
use reqwest::Method;
use serde_json::{json, Value};

use crate::state::AppState;

pub struct Api {
    base: String,
    admin: String,
    pub state: AppState,
    pub pool: Pool,
}

/// Start a daemon on a free port, with one admin token.
pub async fn start() -> Api {
    data_dir_once();
    let pool = errand_core::db::open_memory()
        .await
        .expect("an in-memory database");
    let state = AppState::new(pool.clone());
    let admin = mint(&pool, "test-admin", "admin").await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let addr = listener.local_addr().expect("the port it took");
    let app = super::routes::router(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    Api {
        base: format!("http://{addr}"),
        admin,
        state,
        pool,
    }
}

/// Mint a token with exactly these permissions, the way `POST /v1/tokens` does.
pub async fn mint(pool: &Pool, name: &str, scopes: &str) -> String {
    let token = super::auth::generate_token().expect("system randomness");
    errand_core::db::insert_token(pool, name, &super::auth::hash_token(&token), scopes)
        .await
        .expect("storing the token");
    token
}

/// Playbooks are files on disk, so the tests need a directory of their own
/// rather than the developer's real one.
fn data_dir_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("errand-api-tests-{}", errand_core::new_id()));
        std::env::set_var("ERRAND_DATA_DIR", &dir);
    });
}

impl Api {
    pub async fn as_token(
        &self,
        token: &str,
        method: Method,
        path: &str,
        body: Option<Value>,
        idempotency_key: Option<&str>,
    ) -> (u16, Value) {
        let mut req = reqwest::Client::new()
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(token);
        if let Some(k) = idempotency_key {
            req = req.header("Idempotency-Key", k);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await.expect("the daemon answered");
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    pub async fn get(&self, path: &str) -> Value {
        let admin = self.admin.clone();
        self.as_token(&admin, Method::GET, path, None, None).await.1
    }

    pub async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let admin = self.admin.clone();
        self.as_token(&admin, Method::POST, path, Some(body), None)
            .await
    }

    pub async fn patch(&self, path: &str, body: Value) -> (u16, Value) {
        let admin = self.admin.clone();
        self.as_token(&admin, Method::PATCH, path, Some(body), None)
            .await
    }

    pub async fn patch_with_key(&self, path: &str, body: Value, key: &str) -> (u16, Value) {
        let admin = self.admin.clone();
        self.as_token(&admin, Method::PATCH, path, Some(body), Some(key))
            .await
    }

    pub async fn delete(&self, path: &str) -> (u16, Value) {
        let admin = self.admin.clone();
        self.as_token(&admin, Method::DELETE, path, None, None)
            .await
    }
}

/// Create a task the way the interface does, and hand back its id.
pub async fn a_task(api: &Api, body: Value) -> String {
    let (code, task) = api.post("/v1/tasks", body).await;
    assert_eq!(code, 200, "creating the task failed: {task}");
    task["id"].as_str().expect("a task id").to_string()
}

/// Give a task an approved playbook, which is the one gate between "somebody
/// watched it try once" and "it does this alone at eight in the morning".
///
/// This is the call the executor makes when a teach run finishes and the person
/// approves what it wrote; running a real teach run in a test would need a
/// model and a browser.
pub async fn taught(api: &Api, task_id: &str) {
    let version = errand_core::db::next_playbook_version(&api.pool, task_id)
        .await
        .expect("the next version number");
    let pb = errand_core::playbook::Playbook {
        version,
        goal: "Book a court.".into(),
        sites: vec!["example.com".into()],
        preconditions: vec![],
        steps: vec![errand_core::playbook::Step {
            intent: "Open the booking grid.".into(),
            hint: None,
            decision: None,
        }],
        success: vec![],
        known_failures: vec![],
        never: vec![],
    };
    errand_core::db::add_playbook_version(
        &api.pool,
        task_id,
        &pb,
        errand_core::playbook::Source::Teach,
        None,
        Some("taught in a test"),
        true,
    )
    .await
    .expect("storing the playbook");
}

/// A task that is taught, active, and only ever runs when asked.
pub async fn a_ready_manual_task(api: &Api) -> String {
    let id = a_task(
        api,
        json!({ "name": "Court", "description": "Book the usual court." }),
    )
    .await;
    taught(api, &id).await;
    let (code, body) = api
        .post(&format!("/v1/tasks/{id}/activate"), json!({}))
        .await;
    assert_eq!(code, 200, "activating the task failed: {body}");
    id
}

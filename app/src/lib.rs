//! The window.
//!
//! This shell does almost nothing on purpose. It never executes a task, never
//! opens the database, and never touches a website. It asks the daemon, and the
//! daemon does the work, which is what lets you quit this window without
//! stopping anything that was running.
//!
//! The one job it does take seriously is the API token. Every call is proxied
//! through here so the token stays in Rust: a token in JavaScript is a token in
//! the page, readable by anything that ends up running there, and this one can
//! start runs and read your whole history.

use tauri::Manager;

struct Daemon {
    base: String,
    token: tokio::sync::RwLock<Option<String>>,
}

impl Daemon {
    fn new() -> Self {
        let port = std::env::var("ERRAND_API_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(errand_core::DEFAULT_API_PORT);
        Self {
            // Deliberately the numeric address. `localhost` resolves to IPv6
            // first on some systems, and the daemon listens on IPv4.
            base: format!("http://127.0.0.1:{port}"),
            token: tokio::sync::RwLock::new(None),
        }
    }

    /// The token, read from the keychain once and kept in Rust.
    async fn token(&self) -> Result<String, String> {
        if let Some(t) = self.token.read().await.clone() {
            return Ok(t);
        }
        let fetched = tokio::task::spawn_blocking(|| {
            errand_core::keychain::get_internal(errand_core::keychain::ACCOUNT_API_TOKEN)
                .map(|s| s.expose().to_string())
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|_| {
            json_err(
                "no_token",
                "Errand could not read its own key from your keychain. If you were just asked for \
                 permission and said no, allow it and reopen this window. Otherwise run \
                 'errandd token --new' in a terminal.",
            )
        })?;

        *self.token.write().await = Some(fetched.clone());
        Ok(fetched)
    }
}

fn json_err(code: &str, detail: &str) -> String {
    serde_json::json!({ "code": code, "detail": detail }).to_string()
}

/// Proxy one API call. The webview names a method and a path; it never sees a
/// token, and it cannot reach any host but the daemon.
#[tauri::command]
async fn api(
    state: tauri::State<'_, Daemon>,
    method: String,
    path: String,
    body: Option<String>,
) -> Result<String, String> {
    // The path is joined onto a fixed base, so a path cannot redirect this
    // anywhere else even if something in the page went wrong.
    if !path.starts_with("/v1/") || path.contains("..") {
        return Err(json_err("bad_path", "That is not a valid request."));
    }

    let token = state.token().await?;
    let url = format!("{}{}", state.base, path);
    let client = reqwest::Client::new();

    let mut req = match method.as_str() {
        "GET" => client.get(&url),
        "POST" => client.post(&url),
        "DELETE" => client.delete(&url),
        "PATCH" => client.patch(&url),
        other => {
            return Err(json_err(
                "bad_method",
                &format!("{other} is not supported."),
            ))
        }
    }
    .bearer_auth(token)
    .timeout(std::time::Duration::from_secs(30));

    if let Some(b) = body {
        req = req.header("Content-Type", "application/json").body(b);
    }

    let res = req.send().await.map_err(|e| {
        json_err(
            "unreachable",
            &format!(
                "Errand's background service is not answering ({e}). It normally starts by \
                 itself; if this keeps happening, run 'errandd doctor' in a terminal."
            ),
        )
    })?;

    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    if status.is_success() {
        Ok(text)
    } else {
        Err(if text.trim_start().starts_with('{') {
            text
        } else {
            json_err("http_error", &format!("The service returned {status}."))
        })
    }
}

/// Is the daemon up? Answered without a token, so the window can say something
/// useful even when the keychain is the problem.
#[tauri::command]
async fn daemon_up(state: tauri::State<'_, Daemon>) -> Result<bool, String> {
    Ok(reqwest::Client::new()
        .get(format!("{}/v1/health", state.base))
        .timeout(std::time::Duration::from_secs(4))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(Daemon::new())
        .invoke_handler(tauri::generate_handler![api, daemon_up])
        .setup(|app| {
            // Shown only once the page is ready, so nobody sees a white square.
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("could not start Errand-AI");
}

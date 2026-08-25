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
            errand_core::keychain::get_api_token().map(|s| s.expose().to_string())
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

    /// Drop the held token so the next call reads it afresh from the keychain.
    async fn forget_token(&self) {
        *self.token.write().await = None;
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

    if !matches!(method.as_str(), "GET" | "POST" | "DELETE" | "PATCH") {
        return Err(json_err(
            "bad_method",
            &format!("{method} is not supported."),
        ));
    }

    let (status, text) = send_once(&state, &method, &path, body.clone()).await?;
    if status.is_success() {
        return Ok(text);
    }

    // A 401 usually means this window is holding a token the daemon no longer
    // knows: the daemon minted a new one after this window read the old one.
    // The window can fix that by itself, so it does, once, rather than showing
    // somebody a status code and asking them to restart the app.
    if status == reqwest::StatusCode::UNAUTHORIZED {
        state.forget_token().await;
        let (status, text) = send_once(&state, &method, &path, body).await?;
        if status.is_success() {
            return Ok(text);
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(json_err(
                "token_mismatch",
                "Errand's key and its background service no longer agree, so the service is \
                 refusing this window. This happens after switching between a development and a \
                 release build. Run 'errandd token --new' in a terminal and reopen this window.",
            ));
        }
        return Err(describe(status, text));
    }

    Err(describe(status, text))
}

/// One attempt, with whatever token is currently held.
async fn send_once(
    state: &tauri::State<'_, Daemon>,
    method: &str,
    path: &str,
    body: Option<String>,
) -> Result<(reqwest::StatusCode, String), String> {
    let token = state.token().await?;
    let url = format!("{}{}", state.base, path);
    let client = reqwest::Client::new();

    let mut req = match method {
        "GET" => client.get(&url),
        "POST" => client.post(&url),
        "DELETE" => client.delete(&url),
        _ => client.patch(&url),
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
    Ok((status, res.text().await.unwrap_or_default()))
}

/// The daemon writes its own refusals in plain language, so those are passed
/// through untouched. Anything else gets a sentence rather than a number.
fn describe(status: reqwest::StatusCode, text: String) -> String {
    if text.trim_start().starts_with('{') {
        return text;
    }
    match status.as_u16() {
        404 => json_err("not_found", "That is no longer there."),
        408 | 504 => json_err(
            "timeout",
            "Errand's background service took too long to answer. It may be in the middle of a \
             run; try again in a moment.",
        ),
        500..=599 => json_err(
            "service_error",
            "Something went wrong inside Errand's background service. 'errandd doctor' in a \
             terminal will say what.",
        ),
        _ => json_err(
            "http_error",
            &format!("Errand's background service refused that request ({status})."),
        ),
    }
}

/// Follow a run as it happens.
///
/// The daemon publishes each step as a server-sent event, but the page cannot
/// open that stream itself: doing so would mean handing the token to JavaScript,
/// and this one can start runs and read the whole history. So the stream is held
/// here in Rust and each line is pushed to the page down a channel.
///
/// This replaces a three-second poll. Watching a run is the whole point of
/// teaching a task, and a poll makes a live journal look like a slideshow.
#[tauri::command]
async fn follow_run(
    state: tauri::State<'_, Daemon>,
    run_id: String,
    on_event: tauri::ipc::Channel<String>,
) -> Result<(), String> {
    // The id goes into a URL, so it must be an id and not a path.
    if !run_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(json_err("bad_path", "That is not a valid run."));
    }

    let token = state.token().await?;
    let mut res = reqwest::Client::new()
        .get(format!("{}/v1/runs/{run_id}/stream", state.base))
        .bearer_auth(token)
        .header("Accept", "text/event-stream")
        .send()
        .await
        .map_err(|e| {
            json_err(
                "unreachable",
                &format!("Errand's background service stopped answering ({e})."),
            )
        })?;

    // A frame is an optional `event:` line then a `data:` line, and a network
    // chunk can split either, so the tail is carried over rather than dropped.
    // The two are reassembled here so the page is handed one object with the
    // event's name on it; sending the data alone would leave the page unable to
    // tell a finished step from a failed run.
    let mut buffer = String::new();
    let mut name = String::new();
    while let Some(chunk) = res.chunk().await.map_err(|e| e.to_string())? {
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(nl) = buffer.find('\n') {
            let line: String = buffer.drain(..=nl).collect();
            let line = line.trim_end();

            if let Some(ev) = line.strip_prefix("event:") {
                name = ev.trim().to_string();
                continue;
            }
            // Keep-alive comments start with a colon and mean nothing here.
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() {
                continue;
            }
            let frame = serde_json::json!({
                "event": std::mem::take(&mut name),
                "data": serde_json::from_str::<serde_json::Value>(data)
                    .unwrap_or_else(|_| serde_json::Value::String(data.to_string())),
            });
            if on_event.send(frame.to_string()).is_err() {
                // The page navigated away. Not a failure worth reporting.
                return Ok(());
            }
        }
    }
    Ok(())
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

/// One run artifact, such as a screenshot, handed to the page as a data URL.
///
/// Artifacts are bytes, so they cannot ride the ordinary proxy, which speaks
/// strings. The rule does not change though: the token stays here, the page
/// names an id and nothing more, and a data URL is all it gets back.
#[tauri::command]
async fn artifact(state: tauri::State<'_, Daemon>, id: String) -> Result<String, String> {
    // The id goes into a URL, so it must be an id and not a path.
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(json_err("bad_path", "That is not a valid artifact."));
    }
    let token = state.token().await?;
    let res = reqwest::Client::new()
        .get(format!("{}/v1/artifacts/{id}", state.base))
        .bearer_auth(token)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| {
            json_err(
                "unreachable",
                &format!("Errand's background service is not answering ({e})."),
            )
        })?;
    if !res.status().is_success() {
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        return Err(describe(status, text));
    }
    let mime = res
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/png")
        .to_string();
    let bytes = res.bytes().await.map_err(|e| e.to_string())?;
    use base64::Engine as _;
    Ok(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(Daemon::new())
        .invoke_handler(tauri::generate_handler![
            api, daemon_up, follow_run, artifact
        ])
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

//! The agent's only reach into the world.
//!
//! The executor is spawned with every built-in tool it is possible to remove
//! removed, and with this server as its sole MCP config. Every capability the
//! agent has therefore passes through here, where Rust can check it against the
//! run's task, its domain allowlist, and its budget before anything happens.
//!
//! Served over HTTP at `/mcp/runs/{run_id}` with a bearer token minted for that
//! one run. The token scopes every call to a single run, so a tool call cannot
//! reach another task's data even if the model asks for it.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde_json::{json, Value};

use crate::state::AppState;

/// JSON-RPC error codes we actually use.
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// The tool surface for M2a. Deliberately small: this milestone proves the
/// contained executor loop end to end, so the tools are the ones every run
/// needs regardless of what the task does. Browser control arrives in M2b and
/// slots in here without changing the containment story.
fn tool_definitions() -> Value {
    json!([
        {
            "name": "read_brief",
            "description":
                "Read the task you are carrying out: its name, the user's own description of \
                 the job, and any notes left by previous runs. Call this first.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "journal",
            "description":
                "Record one step in the run journal. The user watches this live and reads it \
                 afterwards, so write a short sentence in plain language describing what you \
                 are doing or deciding, not internal jargon.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "One plain sentence." },
                    "kind": {
                        "type": "string",
                        "enum": ["plan", "read", "decide", "wait", "note"],
                        "description": "What sort of step this is."
                    }
                },
                "required": ["title"],
                "additionalProperties": false
            }
        },
        {
            "name": "finish",
            "description":
                "Finish the run successfully. Provide a one or two sentence summary of what you \
                 actually achieved, written for the person who asked for it.",
            "inputSchema": {
                "type": "object",
                "properties": { "summary": { "type": "string" } },
                "required": ["summary"],
                "additionalProperties": false
            }
        },
        {
            "name": "fail",
            "description":
                "Stop the run because you cannot complete it. Never guess your way past a \
                 blocker, and never pretend a job was done. Answer all three questions plainly: \
                 what you were doing, why you could not finish, and what the person can do next.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "enum": ["auth_expired", "ui_changed", "target_unavailable",
                                 "captcha_or_2fa_needed", "network", "budget_exceeded",
                                 "needs_human_decision", "provider_error"],
                        "description": "Which kind of blocker this is."
                    },
                    "attempting": { "type": "string", "description": "What you were doing." },
                    "because": { "type": "string", "description": "Why you could not finish." },
                    "next_steps": { "type": "string", "description": "What the person can do." }
                },
                "required": ["code", "attempting", "because", "next_steps"],
                "additionalProperties": false
            }
        }        ,
        {
            "name": "open_browser",
            "description":
                "Open the browser for this task. It uses a saved profile per site, so a site you \
                 logged into on a previous run is usually still logged in. Call this before any \
                 other browser tool.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "navigate",
            "description":
                "Go to a URL. Only sites on this task's allowed list will open; anything else is \
                 refused, including a redirect. Returns what is on the page.",
            "inputSchema": {
                "type": "object",
                "properties": { "url": { "type": "string" } },
                "required": ["url"], "additionalProperties": false
            }
        },
        {
            "name": "snapshot",
            "description":
                "Read the current page: its address, title, and the things you can interact with, \
                 each tagged with a ref like [ref=e7]. Take a fresh snapshot after anything that \
                 changes the page, because refs do not survive a reload.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "act",
            "description":
                "Do one thing to the page: click a ref, type text into a ref, select a value, \
                 tick a checkbox, press a key, or scroll. Never type a password with this; use \
                 fill_credential, which is the only way a stored secret reaches a page.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": ["click","type","select","check","press","scroll"] },
                    "ref": { "type": "string", "description": "A ref from the last snapshot, e.g. e7." },
                    "text": { "type": "string" },
                    "value": { "type": "string" },
                    "key": { "type": "string" }
                },
                "required": ["kind"], "additionalProperties": false
            }
        },
        {
            "name": "fill_credential",
            "description":
                "Put a saved credential into a field. You name the credential and the field; you \
                 never see the value, and it is only released to the exact site the credential is \
                 registered against. Use list_credentials to see what this task has.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "credential_id": { "type": "string" },
                    "ref": { "type": "string", "description": "The field to fill, from a snapshot." },
                    "field": { "type": "string", "enum": ["username", "password"], "default": "password" }
                },
                "required": ["credential_id", "ref"], "additionalProperties": false
            }
        },
        {
            "name": "list_credentials",
            "description":
                "List the logins this task may use: their id, label, the site each is bound to, \
                 and the username. Never the secret.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "screenshot",
            "description":
                "Capture what the page looks like, for the person reading this run afterwards. \
                 Password fields are masked before the image is taken.",
            "inputSchema": {
                "type": "object",
                "properties": { "caption": { "type": "string" } },
                "additionalProperties": false
            }
        }
    ])
}

/// Names the agent is permitted to call, as the CLI sees them.
pub fn qualified_tool_names() -> Vec<String> {
    [
        "read_brief",
        "journal",
        "finish",
        "fail",
        "open_browser",
        "navigate",
        "snapshot",
        "act",
        "fill_credential",
        "list_credentials",
        "screenshot",
    ]
    .iter()
    .map(|t| format!("mcp__errand__{t}"))
    .collect()
}

/// How a run ended, as reported by the agent through the tool surface.
#[derive(Debug, Clone)]
pub enum Outcome {
    Finished {
        summary: String,
    },
    Failed {
        code: String,
        attempting: String,
        because: String,
        next_steps: String,
    },
}

impl Outcome {
    /// The three-question explanation the plan requires of every terminal
    /// failure, assembled into the text the user actually reads.
    pub fn failure_human(&self) -> Option<String> {
        match self {
            Outcome::Failed {
                attempting,
                because,
                next_steps,
                ..
            } => Some(format!(
                "**What I was doing:** {attempting}\n\
                 **Why I could not finish:** {because}\n\
                 **What you can do:** {next_steps}"
            )),
            Outcome::Finished { .. } => None,
        }
    }
}

fn rpc_error(id: Value, code: i64, message: &str) -> Json<Value> {
    Json(json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    }))
}

fn rpc_ok(id: Value, result: Value) -> Json<Value> {
    Json(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn text_result(text: impl Into<String>) -> Value {
    json!({ "content": [{ "type": "text", "text": text.into() }], "isError": false })
}

fn text_error(text: impl Into<String>) -> Value {
    json!({ "content": [{ "type": "text", "text": text.into() }], "isError": true })
}

/// POST /mcp/runs/{run_id}
pub async fn handle(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Result<Json<Value>, StatusCode> {
    // The per-run token is what scopes this server to one run.
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .trim()
        .to_string();

    if !state.verify_run_token(&run_id, &presented) {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let msg: Value = serde_json::from_str(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");

    match method {
        "initialize" => Ok(rpc_ok(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "errand", "version": errand_core::VERSION }
            }),
        )),

        // Notifications carry no id and expect no response body.
        m if m.starts_with("notifications/") => Ok(Json(json!({ "jsonrpc": "2.0" }))),

        "tools/list" => Ok(rpc_ok(id, json!({ "tools": tool_definitions() }))),

        "tools/call" => {
            let params = msg.get("params").cloned().unwrap_or(json!({}));
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            let result = dispatch(&state, &run_id, name, &args).await;
            Ok(rpc_ok(id, result))
        }

        "ping" => Ok(rpc_ok(id, json!({}))),

        other => Ok(rpc_error(
            id,
            METHOD_NOT_FOUND,
            &format!("method '{other}' is not supported"),
        )),
    }
}

async fn dispatch(state: &AppState, run_id: &str, name: &str, args: &Value) -> Value {
    match name {
        "read_brief" => match read_brief(state, run_id).await {
            Ok(v) => text_result(v),
            Err(e) => text_error(format!("Could not read the task brief: {e}")),
        },

        "journal" => {
            let Some(title) = args.get("title").and_then(|t| t.as_str()) else {
                return text_error("journal needs a 'title'.");
            };
            let kind = args.get("kind").and_then(|k| k.as_str()).unwrap_or("note");
            // Constrain to the canonical step vocabulary rather than trusting
            // whatever the model sends, since these strings hit a CHECK
            // constraint in the database.
            let kind = match kind {
                "plan" | "read" | "decide" | "wait" | "note" => kind,
                _ => "note",
            };
            match errand_core::db::append_step(state.pool(), run_id, kind, title, true, None).await
            {
                Ok(seq) => {
                    state.emit(errand_core::models::Event::StepFinished {
                        run_id: run_id.to_string(),
                        seq,
                        kind: errand_core::models::StepKind::Note,
                        title: title.to_string(),
                        ok: true,
                        duration_ms: None,
                    });
                    text_result("recorded")
                }
                Err(e) => text_error(format!("could not record that step: {e}")),
            }
        }

        "finish" => {
            let summary = args
                .get("summary")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            if summary.trim().is_empty() {
                return text_error("finish needs a 'summary' describing what you achieved.");
            }
            state.set_outcome(run_id, Outcome::Finished { summary });
            text_result("run recorded as finished")
        }

        "fail" => {
            let get = |k: &str| {
                args.get(k)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string()
            };
            let (code, attempting, because, next_steps) = (
                get("code"),
                get("attempting"),
                get("because"),
                get("next_steps"),
            );
            if attempting.is_empty() || because.is_empty() || next_steps.is_empty() {
                return text_error(
                    "A failure has to answer all three questions: what you were attempting, \
                     why you could not finish, and what the person can do next.",
                );
            }
            state.set_outcome(
                run_id,
                Outcome::Failed {
                    code: if code.is_empty() {
                        "needs_human_decision".into()
                    } else {
                        code
                    },
                    attempting,
                    because,
                    next_steps,
                },
            );
            text_result("run recorded as failed, with your explanation")
        }

        "open_browser" => match open_browser(state, run_id).await {
            Ok(msg) => text_result(msg),
            Err(e) => text_error(format!("Could not open the browser: {e}")),
        },

        "navigate" => {
            let Some(url) = args.get("url").and_then(|u| u.as_str()) else {
                return text_error("navigate needs a 'url'.");
            };
            let Some(b) = state.browser(run_id).await else {
                return text_error("No browser is open. Call open_browser first.");
            };
            match b.goto(url).await {
                Ok(snap) => {
                    let _ = journal(
                        state,
                        run_id,
                        "navigate",
                        &format!("Opened {}", snap.url),
                        true,
                    )
                    .await;
                    // Surface a human check immediately, so the agent fails
                    // honestly rather than burning turns trying to solve
                    // something it is forbidden to solve.
                    if let Ok(Some(kind)) = b.detect_captcha().await {
                        let _ = journal(
                            state,
                            run_id,
                            "decide",
                            &format!("Hit a human verification check ({kind})"),
                            false,
                        )
                        .await;
                        return text_error(format!(
                            "This page is showing a human verification check ({kind}). You must \
                             not attempt to solve or bypass it. Call fail with code \
                             captcha_or_2fa_needed and explain that the person needs to complete \
                             it themselves."
                        ));
                    }
                    text_result(render_snapshot(&snap))
                }
                Err(e) => {
                    // A refused navigation is a decision worth recording: it is
                    // how a redirect to a lookalike site shows up afterwards.
                    let _ = journal(
                        state,
                        run_id,
                        "decide",
                        &format!("Refused to open {url}"),
                        false,
                    )
                    .await;
                    text_error(e.to_string())
                }
            }
        }

        "snapshot" => {
            let Some(b) = state.browser(run_id).await else {
                return text_error("No browser is open. Call open_browser first.");
            };
            match b.snapshot().await {
                Ok(s) => text_result(render_snapshot(&s)),
                Err(e) => text_error(format!("Could not read the page: {e}")),
            }
        }

        "act" => {
            let Some(kind) = args.get("kind").and_then(|k| k.as_str()) else {
                return text_error("act needs a 'kind'.");
            };
            let Some(b) = state.browser(run_id).await else {
                return text_error("No browser is open. Call open_browser first.");
            };
            // Typing a secret through the ordinary path would put it in the
            // journal and the prompt. The only way in is fill_credential.
            if kind == "type" {
                let text = args.get("text").and_then(|t| t.as_str()).unwrap_or("");
                if looks_like_a_secret(text) {
                    return text_error(
                        "That looks like a password. Use fill_credential instead, which keeps \
                         the value out of this conversation entirely.",
                    );
                }
            }
            match b.act(kind, args.clone()).await {
                Ok(_) => {
                    let what = args.get("ref").and_then(|r| r.as_str()).unwrap_or("");
                    let _ =
                        journal(state, run_id, "act", format!("{kind} {what}").trim(), true).await;
                    text_result("done")
                }
                Err(e) => text_error(e.to_string()),
            }
        }

        "list_credentials" => match list_credentials(state, run_id).await {
            Ok(v) => text_result(v),
            Err(e) => text_error(format!("Could not list credentials: {e}")),
        },

        "fill_credential" => match fill_credential(state, run_id, args).await {
            Ok(v) => text_result(v),
            Err(e) => text_error(e.to_string()),
        },

        "screenshot" => {
            let Some(b) = state.browser(run_id).await else {
                return text_error("No browser is open. Call open_browser first.");
            };
            let caption = args
                .get("caption")
                .and_then(|c| c.as_str())
                .unwrap_or("Screenshot");
            match capture(state, run_id, &b, caption).await {
                Ok(_) => text_result("captured"),
                Err(e) => text_error(format!("Could not capture the page: {e}")),
            }
        }

        other => {
            let _ = INVALID_PARAMS;
            text_error(format!("There is no tool called '{other}'."))
        }
    }
}

async fn read_brief(state: &AppState, run_id: &str) -> anyhow::Result<String> {
    let run = errand_core::db::get_run(state.pool(), run_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("run not found"))?;
    let task = errand_core::db::get_task(state.pool(), &run.task_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("task not found"))?;

    Ok(format!(
        "Task: {}\n\nWhat the person asked for:\n{}\n\nThis run: {} (trigger: {})",
        task.name, task.description, run.mode, run.trigger
    ))
}

fn render_snapshot(s: &crate::browser::Snapshot) -> String {
    format!(
        "url: {}\ntitle: {}\n\n{}{}",
        s.url,
        s.title,
        s.tree,
        if s.truncated {
            "\n\n(page truncated; scroll and snapshot again for more)"
        } else {
            ""
        }
    )
}

/// A crude shape check, used only to stop the model routing a secret through
/// the wrong door. False positives cost one redirected tool call.
fn looks_like_a_secret(text: &str) -> bool {
    if text.len() < 8 {
        return false;
    }
    let has_upper = text.chars().any(|c| c.is_ascii_uppercase());
    let has_digit = text.chars().any(|c| c.is_ascii_digit());
    let has_sym = text
        .chars()
        .any(|c| !c.is_alphanumeric() && !c.is_whitespace());
    let no_spaces = !text.contains(' ');
    no_spaces && has_digit && (has_upper || has_sym) && text.len() >= 10
}

fn apex_of(host: &str) -> String {
    let parts: Vec<&str> = host.split('.').filter(|p| !p.is_empty()).collect();
    if parts.len() <= 2 {
        return host.to_string();
    }
    let two = parts[parts.len() - 2..].join(".");
    const TWO_LEVEL: &[&str] = &["co.uk", "org.uk", "ac.uk", "com.au", "co.jp", "com.br"];
    if TWO_LEVEL.contains(&two.as_str()) && parts.len() >= 3 {
        parts[parts.len() - 3..].join(".")
    } else {
        two
    }
}

async fn open_browser(state: &AppState, run_id: &str) -> anyhow::Result<String> {
    if state.browser(run_id).await.is_some() {
        return Ok("The browser is already open.".into());
    }
    let run = errand_core::db::get_run(state.pool(), run_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("run not found"))?;
    let task = errand_core::db::get_task(state.pool(), &run.task_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("task not found"))?;

    let allowed: Vec<String> = task
        .allowed_domains
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    if allowed.is_empty() {
        anyhow::bail!(
            "This task has no approved sites yet, so the browser would not be able to open \
             anything. Add the sites it needs to the task first."
        );
    }

    let apex = apex_of(&allowed[0]);
    let (_pid, dir_name) =
        errand_core::db::claim_browser_profile(state.pool(), &apex, run_id).await?;
    let profile_dir = errand_core::paths::data_root()?.join(dir_name);

    let policy = crate::browser::DomainPolicy {
        allowed: allowed.clone(),
        strict_network: true,
    };
    let b =
        crate::browser::Browser::launch(profile_dir, policy, state.redactor(run_id), true).await?;
    state.set_browser(run_id, std::sync::Arc::new(b)).await;

    let _ = errand_core::db::append_step(
        state.pool(),
        run_id,
        "plan",
        &format!("Opened the browser, limited to: {}", allowed.join(", ")),
        true,
        None,
    )
    .await;

    Ok(format!(
        "Browser open. You may visit: {}. Nothing else will load.",
        allowed.join(", ")
    ))
}

async fn list_credentials(state: &AppState, run_id: &str) -> anyhow::Result<String> {
    let run = errand_core::db::get_run(state.pool(), run_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("run not found"))?;
    let creds = errand_core::db::credentials_for_task(state.pool(), &run.task_id).await?;
    if creds.is_empty() {
        return Ok("This task has no saved logins.".into());
    }
    let mut out = String::from("Logins available to this task:\n");
    for c in creds {
        out.push_str(&format!(
            "- id={} label={:?} site={} username={}\n",
            c.id,
            c.label,
            c.domain,
            c.username.unwrap_or_else(|| "(none)".into())
        ));
    }
    out.push_str("\nUse fill_credential with the id. You will never see the secret itself.");
    Ok(out)
}

async fn fill_credential(state: &AppState, run_id: &str, args: &Value) -> anyhow::Result<String> {
    let cred_id = args
        .get("credential_id")
        .and_then(|c| c.as_str())
        .ok_or_else(|| anyhow::anyhow!("fill_credential needs a 'credential_id'."))?;
    let r#ref = args
        .get("ref")
        .and_then(|r| r.as_str())
        .ok_or_else(|| anyhow::anyhow!("fill_credential needs a 'ref' naming the field."))?;
    let field = args
        .get("field")
        .and_then(|f| f.as_str())
        .unwrap_or("password");

    let run = errand_core::db::get_run(state.pool(), run_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("run not found"))?;

    // The credential must belong to this task. A run cannot reach another
    // task's logins by guessing an id.
    let allowed = errand_core::db::credentials_for_task(state.pool(), &run.task_id).await?;
    let meta = allowed
        .iter()
        .find(|c| c.id == cred_id)
        .ok_or_else(|| anyhow::anyhow!("This task has no credential with id {cred_id}."))?;

    let Some(b) = state.browser(run_id).await else {
        anyhow::bail!("No browser is open. Call open_browser first.");
    };

    // The binding check: the secret is released only to the site it belongs to.
    // This is also what defeats a lookalike page, since a page that merely
    // resembles the real one is on a different domain.
    let snap = b.snapshot().await?;
    let host = url::Url::parse(&snap.url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    let bound = meta.domain.to_ascii_lowercase();
    let host_l = host.to_ascii_lowercase();
    if !(host_l == bound || host_l.ends_with(&format!(".{bound}"))) {
        anyhow::bail!(
            "Refusing to type the {:?} login into {host}. That credential is registered for \
             {bound}, and this page is not it. Nothing was entered.",
            meta.label
        );
    }

    if field == "username" {
        let user = meta
            .username
            .clone()
            .ok_or_else(|| anyhow::anyhow!("That credential has no username saved."))?;
        b.act("type", serde_json::json!({ "ref": r#ref, "text": user }))
            .await?;
    } else {
        let (service, account, _) = errand_core::db::credential_keychain_ref(state.pool(), cred_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("credential metadata missing"))?;
        let secret = crate::secrets::get(service, account).await?;
        b.fill_secret(r#ref, secret.expose(), &meta.label).await?;
    }

    let _ = errand_core::db::mark_credential_used(state.pool(), cred_id).await;
    let _ = errand_core::db::append_step(
        state.pool(),
        run_id,
        "credential",
        &format!("Filled the {} for {:?} into {}", field, meta.label, r#ref),
        true,
        None,
    )
    .await;

    Ok(format!("Filled the {field} for {:?}.", meta.label))
}

async fn capture(
    state: &AppState,
    run_id: &str,
    b: &crate::browser::Browser,
    caption: &str,
) -> anyhow::Result<()> {
    let dir = errand_core::paths::run_dir(run_id)?.join("shots");
    std::fs::create_dir_all(&dir)?;
    let id = errand_core::new_id();
    let path = dir.join(format!("{id}.png"));
    b.screenshot_to(&path).await?;
    let _ =
        errand_core::db::append_step(state.pool(), run_id, "screenshot", caption, true, None).await;
    Ok(())
}

/// Write a journal step, scrubbed, with a last-line assertion that no secret
/// is getting through. The redactor should already have caught it; this is the
/// check that turns a silent leak into a loud one.
async fn journal(
    state: &AppState,
    run_id: &str,
    kind: &str,
    title: &str,
    ok: bool,
) -> anyhow::Result<i64> {
    let red = state.redactor(run_id);
    let clean = red.scrub(title);
    debug_assert!(
        red.is_clean(&clean),
        "a secret survived redaction on its way into the journal"
    );
    if !red.is_clean(&clean) {
        tracing::error!(
            run_id,
            "refusing to journal a line that still contains a secret"
        );
        return errand_core::db::append_step(
            state.pool(),
            run_id,
            kind,
            "[redacted: this step could not be recorded safely]",
            ok,
            None,
        )
        .await;
    }
    errand_core::db::append_step(state.pool(), run_id, kind, &clean, ok, None).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_advertised_tool_has_a_qualified_name() {
        let defs = tool_definitions();
        let names: Vec<&str> = defs
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names.len(), qualified_tool_names().len());
        for n in names {
            assert!(qualified_tool_names().contains(&format!("mcp__errand__{n}")));
        }
    }

    #[test]
    fn a_failure_answers_all_three_questions() {
        let o = Outcome::Failed {
            code: "captcha_or_2fa_needed".into(),
            attempting: "Booking your Wednesday court".into(),
            because: "The site now asks for a code sent to your phone".into(),
            next_steps: "Enter the code once, then press Run now".into(),
        };
        let human = o.failure_human().unwrap();
        assert!(human.contains("What I was doing"));
        assert!(human.contains("Why I could not finish"));
        assert!(human.contains("What you can do"));
    }

    #[test]
    fn success_has_no_failure_text() {
        let o = Outcome::Finished {
            summary: "Court 4 booked".into(),
        };
        assert!(o.failure_human().is_none());
    }
}

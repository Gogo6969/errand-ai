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
                 fill_credential, which is the only way a stored secret reaches a page. \
                 Anything that cannot be undone, such as booking, paying, sending or deleting, \
                 is checked against a safety record first: this run's slot may commit each such \
                 action only once ever, so if you are told it is already done, do not try again \
                 and do not look for another way round it.",
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
        }        ,
        {
            "name": "save_playbook",
            "description":
                "Write down how to do this task, so future runs do not have to work it out again. \
                 Call this once, near the end of a supervised first run, after you know what \
                 actually worked. Describe each step by its INTENT (what you were trying to \
                 achieve, which survives a site redesign) and give the hint (the URL or the \
                 button you used) separately, because hints go stale and intents do not. What \
                 you write is shown to the person for approval before any future run follows it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "goal": { "type": "string", "description": "One sentence: what this task achieves." },
                    "steps": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "intent": { "type": "string", "description": "What this step achieves." },
                                "hint": { "type": "string", "description": "How you did it this time. May go stale." },
                                "decision": { "type": "string", "description": "What to do when the obvious path is not there." }
                            },
                            "required": ["intent"],
                            "additionalProperties": false
                        }
                    },
                    "preconditions": { "type": "array", "items": { "type": "string" } },
                    "success": { "type": "array", "items": { "type": "string" },
                        "description": "How a future run knows it actually worked." },
                    "known_failures": { "type": "array", "items": { "type": "string" } },
                    "never": { "type": "array", "items": { "type": "string" },
                        "description": "Things a future run must never do, such as booking twice." }
                },
                "required": ["goal", "steps"],
                "additionalProperties": false
            }
        },
        {
            "name": "leave_note",
            "description":
                "Leave a short note for the next run of this task: something you learned that is \
                 not worth changing the playbook over, such as a button that moved or a wait that \
                 needed to be longer. Keep it to a sentence or two.",
            "inputSchema": {
                "type": "object",
                "properties": { "note": { "type": "string" } },
                "required": ["note"],
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
        "save_playbook",
        "leave_note",
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
            // Anything that cannot be undone goes through the fence first, so
            // one scheduled occurrence can commit at most one such action, even
            // across crashes, retries, and a differently chosen outcome.
            let the_ref = args.get("ref").and_then(|r| r.as_str()).unwrap_or("");
            let described = b.describe_ref(the_ref).await;
            let action_kind = if kind == "click" {
                described.as_ref().and_then(|(role, label)| {
                    let submits = role.ends_with("+submit");
                    let base = role.trim_end_matches("+submit");
                    crate::browser::classify(base, label, submits)
                })
            } else {
                None
            };

            // A dry run must actually be dry. Enforced here at the tool layer
            // rather than by asking the model nicely, because "I told it not
            // to" is not a guarantee, and a user who trusts a dry run and gets
            // a real booking has been actively misled.
            if let Some(action_kind) = action_kind {
                if is_dry_run(state, run_id).await {
                    let label = described
                        .as_ref()
                        .map(|(_, l)| l.clone())
                        .unwrap_or_default();
                    let _ = journal(
                        state,
                        run_id,
                        "decide",
                        &format!("WOULD HAVE done the {action_kind}: {label:?}"),
                        true,
                    )
                    .await;
                    return text_result(format!(
                        "This is a dry run, so nothing was actually done. Noted that you would \
                         have clicked {label:?} to carry out the {action_kind}. Carry on as if it \
                         had worked, and say in your summary what you would have done."
                    ));
                }
            }

            let mut fence_id: Option<String> = None;
            if let Some(action_kind) = action_kind {
                match guard_irreversible(state, run_id, action_kind).await {
                    Ok(Guard::Proceed(id)) => fence_id = Some(id),
                    Ok(Guard::Refuse(msg)) => {
                        let _ = journal(state, run_id, "decide", &msg, false).await;
                        return text_error(msg);
                    }
                    Err(e) => return text_error(format!("Could not check the safety record: {e}")),
                }
            }

            match b.act(kind, args.clone()).await {
                Ok(_) => {
                    let label = described
                        .as_ref()
                        .map(|(_, l)| l.clone())
                        .unwrap_or_default();
                    let title = if label.is_empty() {
                        format!("{kind} {the_ref}")
                    } else {
                        format!("{kind} {label:?}")
                    };
                    let _ = journal(state, run_id, "act", title.trim(), true).await;

                    if let Some(id) = fence_id {
                        // Commit with evidence, so a later attempt is told what
                        // already happened rather than merely being refused.
                        let url = b.snapshot().await.map(|s| s.url).unwrap_or_default();
                        let evidence = json!({
                            "action": action_kind,
                            "label": label,
                            "url": url,
                            "at": errand_core::now_iso(),
                        });
                        let _ = errand_core::db::commit_side_effect(
                            state.pool(),
                            &id,
                            &evidence.to_string(),
                        )
                        .await;
                        let _ = journal(
                            state,
                            run_id,
                            "decide",
                            &format!(
                                "Recorded that this slot has now had its {} done",
                                action_kind.unwrap_or("action")
                            ),
                            true,
                        )
                        .await;
                    }
                    text_result("done")
                }
                Err(e) => {
                    if let Some(id) = fence_id {
                        // Nothing took effect, so release the slot rather than
                        // leaving it dangling and blocking the task until a
                        // human clears it by hand.
                        let _ = errand_core::db::abort_side_effect(
                            state.pool(),
                            &id,
                            "the action failed before it took effect",
                        )
                        .await;
                    }
                    text_error(e.to_string())
                }
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

        "save_playbook" => match save_playbook(state, run_id, args).await {
            Ok(v) => text_result(v),
            Err(e) => text_error(format!("Could not save the playbook: {e}")),
        },

        "leave_note" => {
            let Some(note) = args.get("note").and_then(|n| n.as_str()) else {
                return text_error("leave_note needs a 'note'.");
            };
            let clean = state.redactor(run_id).scrub(note);
            match errand_core::db::set_run_notes(state.pool(), run_id, &clean).await {
                Ok(_) => text_result("noted for the next run"),
                Err(e) => text_error(format!("could not save that note: {e}")),
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

    let mut out = format!(
        "Task: {}\n\nWhat the person asked for:\n{}\n",
        task.name, task.description
    );

    // The description is the source of truth; the playbook is a shortcut the
    // agent wrote for itself last time. Presented in that order, and labelled,
    // so a stale hint never outranks what the person actually asked for.
    match errand_core::db::active_playbook(state.pool(), &run.task_id).await {
        Ok(Some(pb)) => {
            out.push_str("\nHow this was done before (version ");
            out.push_str(&pb.version.to_string());
            out.push_str(
                "). Treat each INTENT as what matters and each HINT as \
                 how it happened to work last time: if a hint no longer fits the page, pursue the \
                 intent another way and say so in a note.\n\n",
            );
            out.push_str(&pb.to_markdown());
        }
        Ok(None) => {
            out.push_str(
                "\nThere is no playbook for this task yet, so work it out from the description. \
                 When you know what actually worked, call save_playbook so the next run does not \
                 have to start from nothing.\n",
            );
        }
        Err(e) => {
            tracing::warn!(task = %run.task_id, "could not load the playbook: {e}");
        }
    }

    if let Ok(notes) = errand_core::db::recent_notes(state.pool(), &run.task_id, 3).await {
        if !notes.is_empty() {
            out.push_str("\nNotes left by recent runs, oldest first:\n");
            for n in notes {
                out.push_str(&format!("- {n}\n"));
            }
        }
    }

    let mode_note = match run.mode.as_str() {
        "dry_run" => {
            "\nThis is a REHEARSAL. Anything that cannot be undone will be recorded as \
                      what you would have done, and will not actually happen. Work through the \
                      task normally and report what you would have done.\n"
        }
        "teach" => {
            "\nThis is the first, supervised run of this task. Nobody has approved a way \
                    of doing it yet. Work carefully, journal your reasoning as you go, and near \
                    the end call save_playbook with what actually worked.\n"
        }
        _ => "",
    };
    out.push_str(mode_note);
    out.push_str(&format!("\nTriggered by: {}\n", run.trigger));
    Ok(out)
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

/// How recently a matching irreversible action counts as a probable repeat.
/// Long enough to catch a double trigger, short enough that booking again next
/// week is never obstructed.
const REPEAT_WINDOW_MIN: i64 = 10;

enum Guard {
    Proceed(String),
    Refuse(String),
}

/// Ask the fence whether this run may do something irreversible.
async fn guard_irreversible(
    state: &AppState,
    run_id: &str,
    action_kind: &str,
) -> anyhow::Result<Guard> {
    use errand_core::db::FenceVerdict;
    let run = errand_core::db::get_run(state.pool(), run_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("run not found"))?;

    let verdict = errand_core::db::arm_side_effect(
        state.pool(),
        run_id,
        &run.task_id,
        &run.occurrence_id,
        action_kind,
    )
    .await?;

    Ok(match verdict {
        FenceVerdict::Armed(id) => {
            // The fence protects a scheduled slot, but a manual run is its own
            // slot, so pressing Run now twice would otherwise book twice with
            // nothing to stop it. A repeat of the same irreversible action
            // minutes after the last one is almost always an accident, so it
            // stops and asks rather than quietly doing it again.
            if let Some((prev_occ, at, evidence)) = errand_core::db::recent_commit(
                state.pool(),
                &run.task_id,
                action_kind,
                REPEAT_WINDOW_MIN,
            )
            .await?
            {
                if prev_occ != run.occurrence_id {
                    let _ = errand_core::db::abort_side_effect(
                        state.pool(),
                        &id,
                        "a matching action had just been done",
                    )
                    .await;
                    return Ok(Guard::Refuse(format!(
                        "This task already did a {action_kind} at {at}, only minutes ago: {}. \
                         Doing it again now would almost certainly duplicate it, so it has been \
                         stopped. Do not look for another way round this. Report that it appears \
                         to have been done already and finish, so a person can decide whether a \
                         second one was really wanted.",
                        evidence.unwrap_or_else(|| "no details recorded".into())
                    )));
                }
            }
            Guard::Proceed(id)
        }
        FenceVerdict::AlreadyCommitted { evidence } => Guard::Refuse(format!(
            "This run's slot has already had its {action_kind} done: {}. Doing it again would \
             duplicate it. Do not retry. Report what already happened and finish.",
            evidence.unwrap_or_else(|| "no details recorded".into())
        )),
        FenceVerdict::NeedsVerification { armed_at } => Guard::Refuse(format!(
            "An earlier attempt at this slot started a {action_kind} at {armed_at} and never \
             confirmed whether it completed, so it may or may not have gone through. Do not \
             repeat it blindly. Check the site for evidence of whether it already happened, and \
             report what you find. If it plainly did not happen, say so and stop; a human will \
             clear this."
        )),
    })
}

/// Is this run a rehearsal?
async fn is_dry_run(state: &AppState, run_id: &str) -> bool {
    errand_core::db::get_run(state.pool(), run_id)
        .await
        .ok()
        .flatten()
        .map(|r| r.mode == "dry_run")
        .unwrap_or(false)
}

/// Turn what the agent learned into a stored, unapproved playbook version.
///
/// Unapproved on purpose. A playbook is distilled from pages written by
/// strangers and then fed back to the agent as trusted instruction, so a person
/// reads it before any future run follows it.
async fn save_playbook(state: &AppState, run_id: &str, args: &Value) -> anyhow::Result<String> {
    use errand_core::playbook::{Playbook, Source, Step};

    let run = errand_core::db::get_run(state.pool(), run_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("run not found"))?;
    let task = errand_core::db::get_task(state.pool(), &run.task_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("task not found"))?;

    let strings = |k: &str| -> Vec<String> {
        args.get(k)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };

    let steps: Vec<Step> = args
        .get("steps")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| {
                    let intent = s.get("intent")?.as_str()?.to_string();
                    Some(Step {
                        intent,
                        hint: s.get("hint").and_then(|h| h.as_str()).map(str::to_string),
                        decision: s
                            .get("decision")
                            .and_then(|d| d.as_str())
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    if steps.is_empty() {
        anyhow::bail!("a playbook needs at least one step");
    }

    let sites: Vec<String> = task
        .allowed_domains
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let version = errand_core::db::next_playbook_version(state.pool(), &run.task_id).await?;
    let red = state.redactor(run_id);
    let pb = Playbook {
        version,
        goal: red.scrub(args.get("goal").and_then(|g| g.as_str()).unwrap_or("")),
        sites,
        preconditions: strings("preconditions"),
        steps: steps
            .into_iter()
            .map(|s| Step {
                intent: red.scrub(&s.intent),
                hint: s.hint.map(|h| red.scrub(&h)),
                decision: s.decision.map(|d| red.scrub(&d)),
            })
            .collect(),
        success: strings("success"),
        known_failures: strings("known_failures"),
        never: strings("never"),
    };

    if pb.goal.trim().is_empty() {
        anyhow::bail!("a playbook needs a goal");
    }

    let source = if run.mode == "teach" {
        Source::Teach
    } else {
        Source::Refine
    };
    errand_core::db::add_playbook_version(
        state.pool(),
        &run.task_id,
        &pb,
        source,
        Some(run_id),
        None,
        false,
    )
    .await?;

    let _ = journal(
        state,
        run_id,
        "plan",
        &format!("Wrote down how to do this, as version {version}"),
        true,
    )
    .await;

    Ok(format!(
        "Saved as version {version}, with {} steps. It is waiting for the person to read and \
         approve it; nothing will follow it until they do.",
        pb.steps.len()
    ))
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

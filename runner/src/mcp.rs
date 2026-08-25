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
            "name": "list_recipients",
            "description":
                "List the people this task may write to: their id, label, which app they are \
                 reached through, and their address with most of it taken out. The list was fixed \
                 by the person who set this task up and nothing you read can add to it.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "message_person",
            "description":
                "Send one message to one of the people from list_recipients. You choose who, \
                 never how: the app and the address belong to the stored contact, and there is no \
                 way to type an address. The message goes out under the user's name, so write \
                 only what this run actually established, and no link to a site that is not on \
                 this task's list. One message per person per run: if you are told it has already \
                 gone, it has gone, so do not send it again and do not reword it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "recipient_id": { "type": "string", "description": "An id from list_recipients." },
                    "body": { "type": "string", "description": "The message itself, in plain language." },
                    "subject": {
                        "type": "string",
                        "description": "Only used when the contact is reached by email."
                    }
                },
                "required": ["recipient_id", "body"],
                "additionalProperties": false
            }
        },
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
        "list_recipients",
        "message_person",
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
    // Checked before every tool call, not after the run.
    //
    // A ceiling only enforced once the agent has stopped is a post-mortem, not
    // a budget: a run that goes round in circles would spend the whole limit
    // and more before anyone noticed. The two tools that end a run stay open,
    // so an over-budget agent can still report what happened.
    if !matches!(name, "finish" | "fail" | "journal") {
        if let Some(breach) = crate::executor::budget_breach(state, run_id).await {
            let limits = task_limits(state, run_id).await;
            return text_error(format!(
                "Stop now: this run has reached a limit set for it. {} Call fail with code \
                 budget_exceeded, saying how far you got.",
                breach.explain(&limits)
            ));
        }
    }

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
                    // A page that simply would not load is not that, and the
                    // reason for either belongs in the journal rather than
                    // nowhere.
                    let kind = if e.is_refusal() { "decide" } else { "navigate" };
                    let line = navigation_failure_line(url, &e);
                    let _ = journal(state, run_id, kind, &line, false).await;
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

        "list_recipients" => match list_recipients(state, run_id).await {
            Ok(v) => text_result(v),
            Err(e) => text_error(e.to_string()),
        },

        "message_person" => message_person(state, run_id, args).await,

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
                      what you would have done, and will not actually happen. That includes \
                      messages: nothing you send with message_person leaves this machine, and no \
                      person hears from this run. Work through the task normally and report what \
                      you would have done.\n"
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

/// Which browser profile a task's logins live in.
///
/// This has to agree with the allowlist rule, because the profile is claimed
/// from the task's first allowed site: two tasks that come out with the same
/// identity share a profile, and so share whatever is signed in inside it. A
/// hand-written list of two-level endings used to live here, it did not match
/// the one core owns, and so every task on a .co.nz or .co.za address collapsed
/// onto one profile: one person's shop account sitting in the browser another
/// task opens.
///
/// So the question is put to core rather than answered a second time here.
/// `PUBLIC_SUFFIXES` is private to core/src/domains.rs, but the rule built on it
/// is not: `normalize_domain` accepts example.co.uk and refuses a bare co.uk. An
/// ending core will not allow on its own cannot identify a profile either, so
/// one more label is taken. One list, and it lives in core.
fn profile_identity(host: &str) -> String {
    // Not registrable names, and nothing here to shorten. Joining the last two
    // pieces of 127.0.0.1 gives "0.1", which every machine on the network would
    // then share a profile under.
    if host == "localhost" || host.starts_with('[') || host.parse::<std::net::Ipv4Addr>().is_ok() {
        return host.to_string();
    }
    let parts: Vec<&str> = host.split('.').filter(|p| !p.is_empty()).collect();
    if parts.len() <= 2 {
        return host.to_string();
    }
    let two = parts[parts.len() - 2..].join(".");
    match errand_core::domains::normalize_domain(&two) {
        Ok(apex) => apex,
        Err(_) => parts[parts.len() - 3..].join("."),
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

    let apex = profile_identity(&allowed[0]);
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

/// The people this task may write to, as much of them as the agent may see.
///
/// Deliberately the same shape as `list_credentials`: a closed list, chosen by
/// the person, shown by label and never by anything the agent could act on
/// directly. The address is masked because an address the agent never sees is
/// an address it cannot be talked into using.
async fn list_recipients(state: &AppState, run_id: &str) -> anyhow::Result<String> {
    let run = errand_core::db::get_run(state.pool(), run_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("run not found"))?;
    let people = errand_core::db::recipients_for_task(state.pool(), &run.task_id).await?;
    if people.is_empty() {
        anyhow::bail!(
            "This task has nobody it is allowed to write to, so no message can be sent from it \
             at all. If somebody is meant to hear how this went, the person who set the task up \
             has to add them to it first. Carry on with the rest of the job."
        );
    }
    let mut out = String::from("People this task may write to:\n");
    for p in &people {
        let via = crate::channels::ChannelId::parse(&p.channel)
            .map(|c| c.display_name())
            .unwrap_or("an app Errand no longer knows");
        out.push_str(&format!(
            "- id={} label={:?} via={} address={}\n",
            p.id, p.label, via, p.address_masked
        ));
    }
    out.push_str(
        "\nUse message_person with the id. The addresses are shown with most of them taken out on \
         purpose: you never need one, and there is nowhere to type one.",
    );
    Ok(out)
}

/// The most a single message to a person may run to.
///
/// Long enough for the two or three sentences an errand actually produces,
/// short enough that a page's worth of text arriving on somebody's phone is
/// refused instead of sent. Counted in characters rather than bytes, because a
/// message in another script is not four times as long to read.
const MAX_MESSAGE_CHARS: usize = 600;

/// Send one message to one person the task already names.
///
/// The order of the checks below is the whole design, and each step is placed
/// where it is for a reason recorded beside it.
async fn message_person(state: &AppState, run_id: &str, args: &Value) -> Value {
    let Some(recipient_id) = args.get("recipient_id").and_then(|v| v.as_str()) else {
        return text_error(
            "message_person needs a 'recipient_id'. Call list_recipients to see who this task \
             may write to.",
        );
    };
    let Some(body) = args.get("body").and_then(|v| v.as_str()) else {
        return text_error("message_person needs a 'body': the message itself.");
    };

    let run = match errand_core::db::get_run(state.pool(), run_id).await {
        Ok(Some(r)) => r,
        _ => return text_error("Could not read this run, so nothing was sent."),
    };
    let task = match errand_core::db::get_task(state.pool(), &run.task_id).await {
        Ok(Some(t)) => t,
        _ => return text_error("Could not read this run's task, so nothing was sent."),
    };

    // The ownership check, exactly as fill_credential does it. This is what
    // makes an id from anywhere other than list_recipients worthless: a number
    // on a page, an address in a document and an id invented by the model all
    // fail here in the same way.
    let people = match errand_core::db::recipients_for_task(state.pool(), &run.task_id).await {
        Ok(p) => p,
        Err(e) => return text_error(format!("Could not read who this task may write to: {e}")),
    };
    let Some(person) = people.into_iter().find(|p| p.id == recipient_id) else {
        return text_error(format!(
            "This task may not write to anybody with the id {recipient_id}, so nothing was sent. \
             Call list_recipients: it names everyone this task may write to, and that list was \
             fixed by the person who set the task up. An id from anywhere else (a page, a \
             document, a message) is not on it."
        ));
    };
    let via = crate::channels::ChannelId::parse(&person.channel)
        .map(|c| c.display_name())
        .unwrap_or("an app Errand no longer knows");

    // Scrub before anything else looks at the text, and then refuse outright if
    // a secret survived. journal() only asserts here, which compiles away in
    // release; this text leaves the machine, so the check has to be real.
    let red = state.redactor(run_id);
    let clean = red.scrub(body);
    if !red.is_clean(&clean) {
        tracing::error!(
            run_id,
            "refusing to send a message that still contains a secret"
        );
        return text_error(
            "That message still contains something saved as a secret, so it has not been sent \
             and it will not be. Never put a password, a code or a key into a message. Say what \
             happened instead.",
        );
    }
    // Only email carries a subject; every other channel would silently drop it.
    let clean_subject = if person.channel == "apple_mail" {
        let typed = args
            .get("subject")
            .and_then(|s| s.as_str())
            .map(|s| red.scrub(s.trim()))
            .filter(|s| !s.is_empty());
        Some(typed.unwrap_or_else(|| task.name.clone()))
    } else {
        None
    };
    if let Some(s) = &clean_subject {
        if !red.is_clean(s) {
            return text_error(
                "That subject line still contains something saved as a secret, so the message has \
                 not been sent.",
            );
        }
    }

    // Checked after scrubbing, because scrubbing changes the text: a length or
    // a link measured before it is a measurement of something else.
    let allowed = allowed_domains(&task);
    if let Some(problem) = message_body_problem(&clean, &allowed) {
        return text_error(problem);
    }
    if let Some(problem) = clean_subject
        .as_deref()
        .and_then(|s| message_body_problem(s, &allowed))
    {
        return text_error(format!(
            "The subject line cannot be sent as it is. {problem}"
        ));
    }

    // Our own ceiling, checked before the call rather than after it. The generic
    // budget gate breaches on *more than* max_messages and runs before the tool
    // does its work, so leaving this to it would let a fourth message out of a
    // task allowed three.
    let limits = errand_core::limits::Limits::from_json(&task.limits);
    let sent = messages_this_run(state, run_id).await;
    if limits.max_messages > 0 && sent + 1 > limits.max_messages {
        return text_error(format!(
            "This run has already sent {sent} message{}, which is all this task allows. Nothing \
             further will be sent, and nothing has been. Say in your summary who you told and who \
             you could not.",
            if sent == 1 { "" } else { "s" }
        ));
    }

    // Before the fence, never after. A rehearsal that armed the fence would use
    // up this occurrence's one message to this person, and the real run would
    // then be refused for something that never happened.
    if is_dry_run(state, run_id).await {
        let _ = journal(
            state,
            run_id,
            "decide",
            &format!("WOULD HAVE messaged {}: {clean}", person.label),
            true,
        )
        .await;
        return text_result(format!(
            "This is a dry run, so nothing was actually sent and {} heard nothing. Noted that you \
             would have messaged them on {via}. Carry on as if it had been delivered, and say in \
             your summary what you would have sent.",
            person.label
        ));
    }

    let fence = match guard_message(state, &run, &person).await {
        Ok(Guard::Proceed(id)) => id,
        Ok(Guard::Refuse(msg)) => {
            let _ = journal(state, run_id, "decide", &msg, false).await;
            return text_error(msg);
        }
        Err(e) => {
            return text_error(format!(
                "Could not check the record of what this run has already sent, so nothing was \
                 sent: {e}"
            ))
        }
    };

    let queued = errand_core::db::enqueue_message(
        state.pool(),
        errand_core::db::NewMessage {
            run_id: Some(run_id.to_string()),
            task_id: Some(run.task_id.clone()),
            class: "outreach".into(),
            // From the stored contact, never from the arguments. There is no
            // argument for either of these two fields for exactly this reason.
            channel: person.channel.clone(),
            recipient: person.address.clone(),
            recipient_label: Some(person.label.clone()),
            subject: clean_subject,
            body: clean.clone(),
            is_failure: false,
        },
    )
    .await;

    let already_sent = match queued {
        Ok(Some(_)) => false,
        // The outbox dropped it as a repeat of something identical minutes ago.
        // That is a message already on its way to this person, so the fence is
        // committed below rather than released: releasing it would free the slot
        // and let a reworded second attempt through to a real person.
        Ok(None) => true,
        Err(e) => {
            let _ = errand_core::db::abort_side_effect(
                state.pool(),
                &fence,
                "the message could not be queued",
            )
            .await;
            return text_error(format!(
                "That message could not be queued, so nothing was sent and nothing has been \
                 recorded as sent: {e}"
            ));
        }
    };

    // Recorded as a step of kind "message": this is both what the person sees in
    // the run's timeline and what the ceiling above counts.
    let line = if already_sent {
        format!(
            "{} had already been sent this exact message minutes ago, so it was not sent again: \
             {clean}",
            person.label
        )
    } else {
        format!("Messaged {} on {via}: {clean}", person.label)
    };
    let _ = journal(state, run_id, "message", &line, true).await;

    // Committed now, at the point of queueing, not when the message actually
    // goes out. The outbox is a separate worker on a five-second tick, and a
    // fence held armed across it means every crash in between leaves the task
    // needing a person to clear it by hand.
    let evidence = json!({
        "action": "message",
        "recipient": person.label,
        "channel": person.channel,
        "deduplicated": already_sent,
        "at": errand_core::now_iso(),
    });
    if let Err(e) =
        errand_core::db::commit_side_effect(state.pool(), &fence, &evidence.to_string()).await
    {
        // The message is real whether or not this line was written, so this
        // cannot fail the call. It is loud because the next attempt will read a
        // slot that looks free.
        tracing::error!(
            run_id,
            "a message was queued but not recorded on the fence: {e}"
        );
    }

    if already_sent {
        text_result(format!(
            "That exact message to {} went out a few minutes ago, so it has not been sent a \
             second time. Treat them as told. Do not reword it and try again.",
            person.label
        ))
    } else {
        text_result(format!(
            "Queued for {} on {via}. It goes out within a few seconds, unless it is the middle of \
             the night, in which case it waits until morning rather than waking them. That is \
             your one message to them for this run.",
            person.label
        ))
    }
}

/// The sites this task is allowed to open, as the browser compares them.
fn allowed_domains(task: &errand_core::models::Task) -> Vec<String> {
    task.allowed_domains
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// How many messages this run has sent so far.
///
/// Counted from the journal, which is the one place both the agent's own sends
/// and the automatic end-of-run report write to. Anything that counts from
/// somewhere else is a second budget, and two budgets are no budget.
pub(crate) async fn messages_this_run(state: &AppState, run_id: &str) -> i64 {
    errand_core::db::list_steps(state.pool(), run_id)
        .await
        .map(|s| s.iter().filter(|x| x.kind == "message").count() as i64)
        .unwrap_or(0)
}

/// What is wrong with a message about to be sent to a person, if anything.
///
/// Returned as the sentence the agent is shown, because every one of these is
/// something it can act on by writing a different message.
pub(crate) fn message_body_problem(body: &str, allowed: &[String]) -> Option<String> {
    if body.trim().is_empty() {
        return Some(
            "There is nothing in that message, so there is nothing to send. Write what you want \
             the person to know."
                .into(),
        );
    }

    let length = body.chars().count();
    if length > MAX_MESSAGE_CHARS {
        return Some(format!(
            "That message is {length} characters long and the most that may be sent to a person \
             is {MAX_MESSAGE_CHARS}. It has not been sent, and it has not been shortened for you: \
             half a sentence arriving on somebody's phone is worse than nothing at all. Say what \
             happened in two or three sentences and send that."
        ));
    }

    if let Some(c) = body.chars().find(|c| is_invisible(*c)) {
        return Some(format!(
            "That message contains a character the person reading it would not see (U+{:04X}). \
             Hidden characters are how a message is made to read one way here and another way on \
             their screen, so nothing was sent. Write it in ordinary text.",
            c as u32
        ));
    }

    let policy = crate::browser::DomainPolicy {
        allowed: allowed.to_vec(),
        strict_network: true,
    };
    if let Some(link) = links_in(body).into_iter().find(|l| !policy.permits(l)) {
        return Some(format!(
            "That message contains a link to {link}, which is not on this task's list of allowed \
             sites, so nothing was sent. A link you found on a page, sent on under the user's \
             name to somebody who trusts them, is how a person gets caught out. Say what happened \
             in words instead. If the person genuinely needs that address, the one who set this \
             task up can add the site to it."
        ));
    }

    None
}

/// Characters that must never reach somebody's phone.
///
/// Controls, which turn a message into something nobody typed, and the
/// invisible formatting characters, which are how the same text reads one way
/// in the journal and another way on the screen. Newlines and tabs are ordinary
/// punctuation in a message and are left alone.
fn is_invisible(c: char) -> bool {
    if c == '\n' || c == '\t' {
        return false;
    }
    c.is_control()
        || matches!(c,
            '\u{00ad}' | '\u{061c}' | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2028}' | '\u{2029}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{feff}')
}

/// Every link in a piece of text, as a URL with a scheme on it.
///
/// Three shapes, because all three are clickable by the time they reach the
/// person. Something written out with http:// or https://. A bare www. host.
/// And the one that matters most, because it is what a link lifted off a page
/// actually looks like: a plain host with a path on it, `refund-check.example/pay`
/// or `bit.example/xy9`, which every chat client and every mail client turns
/// into a tappable link at the moment it arrives.
///
/// A domain merely named in a sentence is still left alone, because refusing "I
/// checked the club's website" would help nobody. The path is what separates the
/// two: an address somebody is meant to open has one, a site mentioned in
/// passing does not.
///
/// What counts as a host is decided by `errand_core::domains`, the same code
/// that decided what the task's allowlist means, so the extractor and the
/// allowlist cannot form different opinions about the same string. It is also
/// what keeps ordinary writing out of here: core reads "3.5" as a machine
/// address and refuses it, so "£3.50/kg" is a price and not a link.
///
/// Known and deliberate: a bare IP address with a path is not extracted, since
/// the last-label rule is what stops a decimal number reading as a host. A
/// message pointing somebody at one would go out.
fn links_in(text: &str) -> Vec<String> {
    const SCHEMES: &[&str] = &["http://", "https://"];
    let mut out = Vec::new();
    for raw in text.split(|c: char| {
        c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '(' | ')' | '[' | ']')
    }) {
        // Sentence punctuation clings to the end of a pasted link.
        let token = raw.trim_end_matches(['.', ',', ';', ':', '!', '?']);
        if token.is_empty() {
            continue;
        }
        // Lowercasing ASCII never changes a byte's width, so an index into one
        // of these strings is the same index into the other.
        let lower = token.to_ascii_lowercase();

        // Written out with a scheme, wherever in the token it begins.
        if let Some(at) = SCHEMES.iter().filter_map(|m| lower.find(m)).min() {
            let link = &token[at..];
            // A scheme with nothing after it addresses nothing.
            if link
                .split_once("://")
                .is_some_and(|(_, rest)| !rest.is_empty())
            {
                out.push(link.to_string());
            }
            continue;
        }

        // A bare www host, with or without a path. Still found anywhere in the
        // token rather than only at its start, which is how it has always
        // behaved; narrowing it would mean quietly missing a link.
        if let Some(at) = lower.find("www.") {
            let link = &token[at..];
            if link.len() > "www.".len() {
                out.push(format!("https://{link}"));
            }
            continue;
        }

        // An address, not a link. Refusing x@d.example would refuse writing
        // somebody's email address down.
        if token.contains('@') {
            continue;
        }
        let Some(slash) = token.find('/') else {
            continue;
        };
        let (host, path) = token.split_at(slash);
        // A port is not part of the name; core strips it too, and the labels
        // have to be read without it.
        let labels = host
            .split(':')
            .next()
            .unwrap_or_default()
            .trim_end_matches('.');
        if !labels.contains('.') {
            continue;
        }
        // The last label of a real address is letters: .com, .uk, .example.
        // Without this, "3.5/5" and "p.12/13" read as addresses, and a message
        // written in ordinary English never reaches the person it was for.
        let last = labels.rsplit('.').next().unwrap_or_default();
        if last.len() < 2 || !last.chars().all(|c| c.is_ascii_alphabetic()) {
            continue;
        }
        // Core has the final say on whether this is a name at all, and hands
        // back the exact string the allowlist stores, punycode and all. Comparing
        // anything else would be a second opinion about what a domain is.
        let Ok(normalised) = errand_core::domains::normalize_domain(host) else {
            continue;
        };
        out.push(format!("https://{normalised}{path}"));
    }
    out
}

async fn capture(
    state: &AppState,
    run_id: &str,
    b: &crate::browser::Browser,
    caption: &str,
) -> anyhow::Result<()> {
    let dir = errand_core::paths::run_dir(run_id)?.join("shots");
    std::fs::create_dir_all(&dir)?;
    let file = format!("{}.png", errand_core::new_id());
    let path = dir.join(&file);
    b.screenshot_to(&path).await?;

    // The row is what lets the window show the shot afterwards: addressed by
    // id, so asking for it later can never be turned into reading a path the
    // request chose. Journaling stays best-effort, as before.
    let rel = format!("runs/{run_id}/shots/{file}");
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) as i64;
    let artifact =
        errand_core::db::record_artifact(state.pool(), run_id, "screenshot", &rel, bytes)
            .await
            .ok();
    let seq = errand_core::db::append_step(state.pool(), run_id, "screenshot", caption, true, None)
        .await
        .ok();
    if let (Some(a), Some(s)) = (artifact, seq) {
        let _ = errand_core::db::attach_step_artifact(state.pool(), run_id, s, &a).await;
    }
    Ok(())
}

/// Write a journal step, scrubbed, with a last-line assertion that no secret
/// is getting through. The redactor should already have caught it; this is the
/// check that turns a silent leak into a loud one.
/// How a navigation that did not happen reads in the run's journal.
///
/// It used to say "Refused to open {url}" whatever had gone wrong, so a site
/// that was merely down read as a site the person had banned, and the actual
/// reason was dropped on the floor. Two things had to be true of the
/// replacement: a refusal says so and names the site, and anything else reads
/// as a page that would not open.
fn navigation_failure_line(url: &str, e: &crate::browser::NavError) -> String {
    use crate::browser::NavError;
    match e {
        NavError::NotAllowed { .. } => format!(
            "Refused to open {}: it is not on this task's list of allowed sites",
            e.site()
        ),
        NavError::Lookalike { similar, .. } => format!(
            "Refused to open {}: it is not on this task's list of allowed sites, and it looks \
             like {similar}, which is on it",
            e.site()
        ),
        NavError::Failed(why) => {
            // Whole first line, capped: a sidecar error can run to a page of
            // stack, and a journal entry is read on a phone.
            let reason: String = why
                .to_string()
                .lines()
                .next()
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect();
            format!("Could not open {url}: {reason}")
        }
    }
}

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

/// Was this person written to a few minutes ago, for a different occurrence?
///
/// A scheduled slot is protected by the fence itself, but pressing Run now twice
/// mints a fresh slot each time, so nothing else would stop the same person being
/// told the same thing twice in a minute.
///
/// Both message paths ask through here rather than each keeping its own copy of
/// the question: the agent's own `message_person`, and the automatic end-of-run
/// report in the outbox. Only one of them had the check, which is precisely how
/// the gap opened, so a second copy of the window or the query is the thing to
/// avoid. Returns when it happened and what was recorded, for whoever has to
/// explain it afterwards.
pub(crate) async fn messaged_moments_ago(
    state: &AppState,
    task_id: &str,
    person_id: &str,
    occurrence_id: &str,
) -> anyhow::Result<Option<(String, Option<String>)>> {
    let recent = errand_core::db::recent_commit(
        state.pool(),
        task_id,
        "message",
        person_id,
        REPEAT_WINDOW_MIN,
    )
    .await?;
    match recent {
        Some((prev_occurrence, at, evidence)) if prev_occurrence != occurrence_id => {
            Ok(Some((at, evidence)))
        }
        _ => Ok(None),
    }
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
        "",
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
                "",
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

/// Ask the fence whether this run may message this person.
///
/// The same mechanism as `guard_irreversible`, keyed by the recipient as well
/// as the occurrence. Two things depend on that discriminator. The browser
/// classifier already calls a click on anything labelled send, post, publish or
/// reply a "message", so without it a web Send button and this tool would fight
/// over one slot. And messaging two people about one occurrence has to be
/// possible, while messaging one person twice must not be.
///
/// Keying on the recipient is safe in the way keying on an outcome is not: the
/// set of recipients is fixed by the person, and the agent cannot add to it.
async fn guard_message(
    state: &AppState,
    run: &errand_core::models::Run,
    person: &errand_core::models::TaskRecipient,
) -> anyhow::Result<Guard> {
    use errand_core::db::FenceVerdict;
    let verdict = errand_core::db::arm_side_effect(
        state.pool(),
        &run.id,
        &run.task_id,
        &run.occurrence_id,
        "message",
        &person.id,
    )
    .await?;

    Ok(match verdict {
        FenceVerdict::Armed(id) => {
            if let Some((at, evidence)) =
                messaged_moments_ago(state, &run.task_id, &person.id, &run.occurrence_id).await?
            {
                let _ = errand_core::db::abort_side_effect(
                    state.pool(),
                    &id,
                    "this person had just been messaged",
                )
                .await;
                return Ok(Guard::Refuse(format!(
                    "This task messaged {} at {at}, only minutes ago: {}. Writing to them again \
                     now would almost certainly repeat it, so it has been stopped. Do not look \
                     for another way round this. Report that they appear to have been told \
                     already and carry on.",
                    person.label,
                    evidence.unwrap_or_else(|| "no details recorded".into())
                )));
            }
            Guard::Proceed(id)
        }
        FenceVerdict::AlreadyCommitted { evidence } => Guard::Refuse(format!(
            "{} has already been messaged for this run of the task: {}. Sending again would mean \
             they hear it twice. Do not send it again, do not reword it, and do not look for \
             another way round this. Report what has already gone and carry on.",
            person.label,
            evidence.unwrap_or_else(|| "no details recorded".into())
        )),
        FenceVerdict::NeedsVerification { armed_at } => Guard::Refuse(format!(
            "An earlier attempt at this slot started a message to {} at {armed_at} and never \
             confirmed whether it went out, so they may or may not have it. Do not send it again \
             in case they do. Do not look for another way round this. Say plainly in your summary \
             that a message to them is unaccounted for, so a person can check.",
            person.label
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

async fn task_limits(state: &AppState, run_id: &str) -> errand_core::limits::Limits {
    let Ok(Some(run)) = errand_core::db::get_run(state.pool(), run_id).await else {
        return Default::default();
    };
    errand_core::db::get_task(state.pool(), &run.task_id)
        .await
        .ok()
        .flatten()
        .map(|t| errand_core::limits::Limits::from_json(&t.limits))
        .unwrap_or_default()
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
    fn a_refused_navigation_says_which_site_and_why() {
        // The line used to be "Refused to open {url}" with the reason thrown
        // away, so the person read a refusal with no explanation at all.
        let line = navigation_failure_line(
            "https://not-allowed.example/x",
            &crate::browser::NavError::NotAllowed {
                url: "https://not-allowed.example/x".into(),
                allowed: vec!["tennis-club.example".into()],
            },
        );
        assert!(line.contains("Refused"), "{line}");
        assert!(line.contains("not-allowed.example"), "{line}");
        assert!(
            line.contains("list of allowed sites"),
            "the reason has to be in the line the person reads: {line}"
        );
    }

    #[test]
    fn a_lookalike_site_is_named_alongside_the_one_it_imitates() {
        let line = navigation_failure_line(
            "https://tennls-club.example/login",
            &crate::browser::NavError::Lookalike {
                url: "https://tennls-club.example/login".into(),
                similar: "tennis-club.example".into(),
            },
        );
        assert!(line.contains("tennls-club.example"), "{line}");
        assert!(line.contains("tennis-club.example"), "{line}");
    }

    #[test]
    fn a_page_that_would_not_load_is_not_reported_as_a_refusal() {
        // Every goto failure used to be journalled with the word "Refused",
        // which accuses the allowlist of something it did not do.
        let line = navigation_failure_line(
            "https://tennis-club.example/book",
            &crate::browser::NavError::Failed(anyhow::anyhow!(
                "net::ERR_CONNECTION_REFUSED at https://tennis-club.example/book"
            )),
        );
        assert!(
            !line.contains("Refused to open"),
            "a site being down is not Errand refusing to go there: {line}"
        );
        assert!(
            line.contains("ERR_CONNECTION_REFUSED"),
            "the actual reason was being dropped entirely: {line}"
        );
        assert!(line.contains("tennis-club.example/book"), "{line}");
    }

    #[test]
    fn only_the_allowlist_refusals_count_as_a_decision() {
        // The journal kind decides how the step reads back: a decision Errand
        // made, or a step that did not work.
        assert!(crate::browser::NavError::NotAllowed {
            url: "https://x.example/".into(),
            allowed: vec![],
        }
        .is_refusal());
        assert!(!crate::browser::NavError::Failed(anyhow::anyhow!("timed out")).is_refusal());
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

    // ------------------------------------------------- messaging a real person --

    /// A run of a task, set up the way the app sets one up, with the tool server
    /// answering on a real port and a real per-run bearer.
    struct Errand {
        api: crate::api::testkit::Api,
        task_id: String,
        run: errand_core::models::Run,
        token: String,
    }

    impl Errand {
        /// Call a tool the way the CLI calls it: JSON-RPC over the run's own
        /// endpoint, with the run's own token.
        async fn call(&self, tool: &str, args: Value) -> (bool, String) {
            let (status, body) = self
                .api
                .as_token(
                    &self.token,
                    reqwest::Method::POST,
                    &format!("/mcp/runs/{}", self.run.id),
                    Some(json!({
                        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                        "params": { "name": tool, "arguments": args }
                    })),
                    None,
                )
                .await;
            assert_eq!(status, 200, "the tool server refused the call: {body}");
            let result = &body["result"];
            (
                result["isError"].as_bool().unwrap_or(false),
                result["content"][0]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
        }

        /// Somebody the task may write to, added and granted the way the
        /// settings screen does it.
        async fn may_write_to(&self, label: &str, channel: &str, address: &str) -> String {
            let (code, person) = self
                .api
                .post(
                    "/v1/recipients",
                    json!({ "label": label, "channel": channel, "address": address }),
                )
                .await;
            assert_eq!(code, 200, "saving the contact failed: {person}");
            let id = person["id"].as_str().expect("a contact id").to_string();
            let (code, body) = self
                .api
                .post(
                    &format!("/v1/tasks/{}/recipients", self.task_id),
                    json!({ "recipient_id": id }),
                )
                .await;
            assert_eq!(code, 200, "granting the task access failed: {body}");
            id
        }

        async fn queued_messages(&self) -> Vec<errand_core::db::OutboxRow> {
            errand_core::db::due_outbox(&self.api.pool, 50)
                .await
                .expect("the outbox")
                .into_iter()
                .filter(|r| r.class == "outreach")
                .collect()
        }
    }

    async fn an_errand(mode: &str, limits: Value) -> Errand {
        let api = crate::api::testkit::start().await;
        let task_id = crate::api::testkit::a_task(
            &api,
            json!({
                "name": "Order the shopping",
                "description": "Put the usual order in.",
                "limits": limits,
                "allowed_domains": ["shop.example"]
            }),
        )
        .await;
        let run = errand_core::db::try_create_run(
            &api.pool,
            &task_id,
            &format!("manual/{}", errand_core::new_id()),
            "manual",
            mode,
            None,
        )
        .await
        .expect("a run");
        let token = api.state.mint_run_token(&run.id);
        Errand {
            api,
            task_id,
            run,
            token,
        }
    }

    #[tokio::test]
    async fn a_rehearsal_tells_nobody_and_still_reports_that_it_worked() {
        let errand = an_errand("dry_run", json!({})).await;
        let mum = errand
            .may_write_to("Mum", "whatsapp", "+447700900123")
            .await;

        let (is_error, text) = errand
            .call(
                "message_person",
                json!({ "recipient_id": mum, "body": "The shopping is booked for Friday." }),
            )
            .await;

        assert!(
            !is_error,
            "a dry run must succeed, or the agent will go looking for another way: {text}"
        );
        assert!(
            text.contains("nothing was actually sent"),
            "it must say plainly that nobody heard: {text}"
        );
        assert!(
            errand.queued_messages().await.is_empty(),
            "a rehearsal put a real message in the queue"
        );

        // And the slot is untouched, so the real run is not refused for
        // something that never happened.
        let armed = errand_core::db::arm_side_effect(
            &errand.api.pool,
            &errand.run.id,
            &errand.task_id,
            &errand.run.occurrence_id,
            "message",
            &mum,
        )
        .await
        .expect("asking the fence");
        assert!(
            matches!(armed, errand_core::db::FenceVerdict::Armed(_)),
            "a rehearsal used up the one message this slot may send"
        );
    }

    #[tokio::test]
    async fn an_id_this_task_was_never_given_reaches_nobody() {
        let errand = an_errand("normal", json!({})).await;
        // Somebody who exists, but whom this task was never granted.
        let (_, stranger) = errand
            .api
            .post(
                "/v1/recipients",
                json!({ "label": "A stranger", "channel": "apple_mail",
                        "address": "stranger@example.com" }),
            )
            .await;
        let stranger_id = stranger["id"].as_str().expect("a contact id");

        for id in [stranger_id, "made-up-id"] {
            let (is_error, text) = errand
                .call(
                    "message_person",
                    json!({ "recipient_id": id, "body": "Please confirm the order." }),
                )
                .await;
            assert!(is_error, "an id from nowhere was accepted: {text}");
            assert!(
                text.contains("list_recipients"),
                "the refusal must say where the real list is: {text}"
            );
        }
        assert!(
            errand.queued_messages().await.is_empty(),
            "a message went out to somebody this task may not write to"
        );
    }

    #[tokio::test]
    async fn a_link_to_a_site_this_task_does_not_use_is_not_passed_on() {
        let errand = an_errand("normal", json!({})).await;
        let mum = errand
            .may_write_to("Mum", "whatsapp", "+447700900123")
            .await;

        let (is_error, text) = errand
            .call(
                "message_person",
                json!({ "recipient_id": mum,
                        "body": "The order is in. Confirm it here: https://claim-your-refund.example/x" }),
            )
            .await;
        assert!(
            is_error,
            "a link to a site nobody approved was sent: {text}"
        );
        assert!(
            text.contains("claim-your-refund.example"),
            "the refusal must name the link it refused: {text}"
        );
        assert!(errand.queued_messages().await.is_empty());

        // A link to a site the task actually uses is ordinary.
        let (is_error, text) = errand
            .call(
                "message_person",
                json!({ "recipient_id": mum,
                        "body": "The order is in: https://shop.example/orders/8." }),
            )
            .await;
        assert!(
            !is_error,
            "a link to the task's own site was refused: {text}"
        );
        assert_eq!(errand.queued_messages().await.len(), 1);
    }

    #[tokio::test]
    async fn the_message_budget_stops_at_the_number_set_and_not_one_past_it() {
        // The generic budget gate breaches on *more than* the limit and runs
        // before the tool does anything, so a task allowed one message would
        // otherwise get two out before anything noticed.
        let errand = an_errand("normal", json!({ "max_messages": 1 })).await;
        let mum = errand
            .may_write_to("Mum", "whatsapp", "+447700900123")
            .await;
        let dad = errand
            .may_write_to("Dad", "imessage", "+447700900124")
            .await;

        let (is_error, text) = errand
            .call(
                "message_person",
                json!({ "recipient_id": mum, "body": "The shopping is ordered." }),
            )
            .await;
        assert!(!is_error, "the first message was refused: {text}");

        let (is_error, text) = errand
            .call(
                "message_person",
                json!({ "recipient_id": dad, "body": "The shopping is ordered." }),
            )
            .await;
        assert!(is_error, "a second message went out past the limit: {text}");
        assert!(
            text.contains("all this task allows"),
            "the refusal must say it is a limit that was set: {text}"
        );
        assert_eq!(
            errand.queued_messages().await.len(),
            1,
            "exactly one message may leave a task allowed one"
        );
    }

    #[tokio::test]
    async fn one_person_hears_once_while_another_can_still_be_told() {
        let errand = an_errand("normal", json!({ "max_messages": 5 })).await;
        let mum = errand
            .may_write_to("Mum", "whatsapp", "+447700900123")
            .await;
        let dad = errand
            .may_write_to("Dad", "imessage", "+447700900124")
            .await;

        let (is_error, text) = errand
            .call(
                "message_person",
                json!({ "recipient_id": mum, "body": "The shopping is ordered." }),
            )
            .await;
        assert!(!is_error, "{text}");

        // Same person, different words: still the same person, still one run.
        let (is_error, text) = errand
            .call(
                "message_person",
                json!({ "recipient_id": mum, "body": "Just to say the order went through." }),
            )
            .await;
        assert!(is_error, "the same person was messaged twice: {text}");
        assert!(
            text.contains("not look for another way round this"),
            "the refusal must close the door rather than invite a workaround: {text}"
        );

        // Somebody else has heard nothing yet, so they may still be told.
        let (is_error, text) = errand
            .call(
                "message_person",
                json!({ "recipient_id": dad, "body": "The shopping is ordered." }),
            )
            .await;
        assert!(
            !is_error,
            "messaging a second person about one occurrence was blocked: {text}"
        );

        let queued = errand.queued_messages().await;
        assert_eq!(queued.len(), 2, "expected one message each: {queued:?}");
    }

    #[tokio::test]
    async fn the_report_at_the_end_does_not_repeat_what_the_agent_already_said() {
        // Two paths reach the same person: the agent's own tool during the run,
        // and the automatic report afterwards. One person, one run, one message,
        // whichever path gets there first.
        let errand = an_errand("normal", json!({ "max_messages": 5 })).await;
        let mum = errand
            .may_write_to("Mum", "whatsapp", "+447700900123")
            .await;

        let (is_error, text) = errand
            .call(
                "message_person",
                json!({ "recipient_id": mum, "body": "The shopping is ordered for Friday." }),
            )
            .await;
        assert!(!is_error, "{text}");

        errand_core::db::finish_run_ok(&errand.api.pool, &errand.run.id, "Ordered for Friday.")
            .await
            .expect("finishing the run");
        crate::outbox::notify_run(&errand.api.state, &errand.run.id)
            .await
            .expect("queueing the reports");

        assert_eq!(
            errand.queued_messages().await.len(),
            1,
            "she was written to twice about one run"
        );
    }

    #[tokio::test]
    async fn a_task_with_nobody_to_write_to_is_told_what_the_person_must_do() {
        let errand = an_errand("normal", json!({})).await;
        let (is_error, text) = errand.call("list_recipients", json!({})).await;
        assert!(
            is_error,
            "an empty list must be a refusal, not a blank list"
        );
        assert!(
            text.contains("has to add them to it first"),
            "it must name what the person has to do: {text}"
        );
    }

    #[tokio::test]
    async fn the_agent_is_shown_enough_to_recognise_somebody_never_enough_to_reach_them() {
        let errand = an_errand("normal", json!({})).await;
        errand
            .may_write_to("Mum", "apple_mail", "mum@example.com")
            .await;

        let (is_error, text) = errand.call("list_recipients", json!({})).await;
        assert!(!is_error, "{text}");
        assert!(text.contains("Mum"), "{text}");
        assert!(text.contains("Apple Mail"), "{text}");
        assert!(
            !text.contains("mum@example.com"),
            "the full address must never be shown to the agent: {text}"
        );
    }

    #[test]
    fn a_message_that_would_arrive_half_finished_is_refused_rather_than_cut() {
        let allowed = vec!["shop.example".to_string()];
        let long = "a".repeat(MAX_MESSAGE_CHARS + 1);
        let problem = message_body_problem(&long, &allowed).expect("an over-long message");
        assert!(
            problem.contains("has not been shortened"),
            "it must say it refused rather than truncated: {problem}"
        );
        // Counted in characters, not bytes, so a message in another script is
        // not refused for being written in that script.
        let accented = "é".repeat(MAX_MESSAGE_CHARS);
        assert!(message_body_problem(&accented, &allowed).is_none());
    }

    #[test]
    fn text_the_reader_would_never_see_is_refused() {
        let allowed = vec!["shop.example".to_string()];
        assert!(message_body_problem("Ordered\u{200b}for Friday", &allowed).is_some());
        assert!(message_body_problem("Ordered\u{202e}for Friday", &allowed).is_some());
        assert!(message_body_problem("Ordered\u{7}", &allowed).is_some());
        // Newlines and tabs are ordinary punctuation in a message.
        assert!(message_body_problem("Ordered.\nIt arrives Friday.", &allowed).is_none());
    }

    #[test]
    fn a_link_is_found_however_it_is_written() {
        let found = links_in(
            "see https://a.example/x, or www.b.example. Not c.example. But c.example/x is one. \
             mail me at x@d.example (https://e.example)",
        );
        assert_eq!(
            found,
            vec![
                "https://a.example/x",
                "https://www.b.example",
                "https://c.example/x",
                "https://e.example"
            ],
            "a site merely named in a sentence is not a link; the same name with a path on it is \
             exactly what a chat client makes tappable"
        );
    }

    /// Bodies that must never leave, and bodies that must.
    ///
    /// Both halves matter equally, and the second half is the one that is easy
    /// to forget: a link that gets through sends a person to a page a stranger
    /// chose, and a sentence wrongly read as a link is a message that never
    /// reaches them at all.
    #[test]
    fn an_address_a_client_would_make_tappable_is_refused_and_ordinary_writing_is_not() {
        let allowed = vec!["shop.example".to_string(), "tennis-club.co.uk".to_string()];

        // Each of these arrives on a phone as something to tap. The second
        // column is the address the person would have been sent to.
        let must_not_go_out = [
            (
                "The order needs confirming at secure-refund-check.example/verify",
                "secure-refund-check.example",
            ),
            ("Track the parcel here: bit.example/xy9", "bit.example"),
            ("Confirm at EVIL.example/pay before Friday", "evil.example"),
            ("Details are at evil.example:8443/pay", "evil.example"),
            (
                "Sort it out (refund-check.example/now)",
                "refund-check.example",
            ),
            ("Pay the balance at evil.example/", "evil.example"),
            ("See https://evil.example/pay", "evil.example"),
            ("See www.evil.example", "www.evil.example"),
            (
                "It moved to shop.example.evil.example/basket",
                "shop.example.evil.example",
            ),
        ];
        for (body, address) in must_not_go_out {
            let problem = message_body_problem(body, &allowed)
                .unwrap_or_else(|| panic!("this would have gone out exactly as it is: {body:?}"));
            assert!(
                problem.contains(address),
                "the refusal has to name the address it found, or nobody can act on it: {problem}"
            );
        }

        // Ordinary sentences. Every one of these has to reach the person.
        let must_go_out = [
            "I booked it. Done.",
            "The shopping is ordered. It arrives Friday.",
            "It came to £3.50/kg, which is up on last week.",
            "The reviews rate it 3.5/5, so I went ahead.",
            "Ready Mon/Tue, whichever suits.",
            "We split it 50/50.",
            "See p.12/13 of the booklet they sent.",
            "Ask for Dr Smith w/ the referral letter.",
            "The reference is AB.12/C if they ask.",
            "e.g./i.e. either wording is fine.",
            "Open 24/7 over the bank holiday.",
            "I checked the club's website, tennis-club.co.uk, and it was fine.",
            "Write to mum@shop.example if any of that is wrong.",
            // On the task's own list, so it is allowed to be there.
            "It is all on shop.example/basket if you want a look.",
            "See https://shop.example/basket",
            "Court 4 is booked at bookings.tennis-club.co.uk/court4.",
        ];
        for body in must_go_out {
            assert!(
                message_body_problem(body, &allowed).is_none(),
                "an ordinary sentence was refused, so the person was told nothing at all: \
                 {body:?} -> {:?}",
                message_body_problem(body, &allowed)
            );
        }
    }

    // ------------------------------------------- which browser profile is used --

    #[test]
    fn two_sites_that_share_only_an_ending_do_not_share_a_browser_profile() {
        // What this replaces: a list written out here by hand had no .co.nz in
        // it, so both of these came out as "co.nz" and one household's shop
        // account sat signed in inside the browser the other task opened.
        assert_eq!(profile_identity("tennis-club.co.nz"), "tennis-club.co.nz");
        assert_ne!(
            profile_identity("tennis-club.co.nz"),
            profile_identity("powershop.co.nz")
        );
        assert_ne!(
            profile_identity("bank.co.za"),
            profile_identity("shop.co.za")
        );
    }

    #[test]
    fn a_subdomain_uses_the_profile_its_own_site_is_signed_in_to() {
        assert_eq!(profile_identity("example.com"), "example.com");
        assert_eq!(profile_identity("shop.eu.example.com"), "example.com");
        assert_eq!(
            profile_identity("bookings.tennis-club.co.uk"),
            "tennis-club.co.uk"
        );
    }

    #[test]
    fn a_machine_address_is_not_shortened_into_something_every_machine_shares() {
        // "0.1" is what 127.0.0.1 used to be filed under, along with everything
        // else whose address happens to end that way.
        assert_eq!(profile_identity("127.0.0.1"), "127.0.0.1");
        assert_eq!(profile_identity("localhost"), "localhost");
        assert_eq!(profile_identity("[::1]"), "[::1]");
    }

    #[test]
    fn the_browser_profile_and_the_allowlist_never_disagree_about_where_a_name_begins() {
        // Each kept its own list of the endings that belong to everybody rather
        // than to somebody, and the two did not match. This asserts the property
        // rather than the list, so it goes on holding when core's list changes.
        for ending in [
            "co.uk", "org.uk", "ac.uk", "com.au", "co.jp", "com.br", "co.nz", "co.za", "gov.uk",
            "com", "example",
        ] {
            let host = format!("shop.example.{ending}");
            let core_refuses_the_bare_ending =
                errand_core::domains::normalize_domain(ending).is_err();
            let profile_keeps_the_whole_name =
                profile_identity(&host) == format!("example.{ending}");
            assert_eq!(
                core_refuses_the_bare_ending, profile_keeps_the_whole_name,
                "'{ending}': the allowlist and the browser profile disagree about whether anybody \
                 can register under this, which is how two unrelated tasks came to share one \
                 browser"
            );
        }
    }

    #[test]
    fn success_has_no_failure_text() {
        let o = Outcome::Finished {
            summary: "Court 4 booked".into(),
        };
        assert!(o.failure_human().is_none());
    }
}

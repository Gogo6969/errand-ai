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
        }
    ])
}

/// Names the agent is permitted to call, as the CLI sees them.
pub fn qualified_tool_names() -> Vec<String> {
    ["read_brief", "journal", "finish", "fail"]
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

        other => {
            let _ = INVALID_PARAMS;
            text_error(format!(
                "There is no tool called '{other}'. You have: read_brief, journal, finish, fail."
            ))
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

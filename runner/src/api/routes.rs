//! Route handlers.
//!
//! Scope checks are explicit on every mutating route rather than inferred from
//! the path, so adding a route cannot accidentally inherit someone else's
//! permissions.

use axum::extract::{Extension, Path, Query, State};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use errand_core::models::{Event, Scope};
use serde::Deserialize;
use serde_json::json;

use super::auth::{require, Caller};
use super::{ApiError, ApiResult};
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    // Unauthenticated: liveness only, and it reveals nothing beyond "alive".
    //
    // The MCP endpoint sits here rather than behind the API-token middleware
    // because it carries its own per-run bearer, checked inside the handler.
    // A run's token grants access to that run alone, and to nothing else.
    let public = Router::new()
        .route("/v1/health", get(health_public))
        .route("/mcp/runs/{run_id}", post(crate::mcp::handle));

    let private = Router::new()
        .route("/v1/health/detail", get(health_detail))
        .route("/v1/tasks", get(list_tasks).post(create_task))
        .route("/v1/tasks/{id}", get(get_task))
        .route("/v1/tasks/{id}/activate", post(activate_task))
        .route("/v1/tasks/{id}/teach", post(teach_task))
        .route("/v1/tasks/{id}/playbook", get(get_playbook))
        .route(
            "/v1/tasks/{id}/playbook/{version}/approve",
            post(approve_playbook),
        )
        .route("/v1/tasks/{id}/holds", post(resolve_holds))
        .route("/v1/tasks/{id}/pause", post(pause_task))
        .route("/v1/tasks/{id}/resume", post(resume_task))
        .route("/v1/tasks/{id}/run", post(run_task))
        .route("/v1/runs", get(list_runs))
        .route("/v1/runs/{id}", get(get_run))
        .route("/v1/runs/{id}/stream", get(super::sse::run_stream))
        .route("/v1/events", get(super::sse::global_stream))
        .route(
            "/v1/credentials",
            get(list_credentials).post(create_credential),
        )
        .route("/v1/credentials/{id}", delete(delete_credential))
        .route("/v1/channels", get(list_channels))
        .route("/v1/channels/{channel}/config", post(configure_channel))
        .route("/v1/channels/{channel}/test", post(test_channel))
        .route("/v1/channels/{channel}/enable", post(enable_channel))
        .route("/v1/tokens", get(list_tokens).post(mint_token))
        .route("/v1/tokens/{id}", delete(revoke_token))
        .route("/v1/webhooks", get(list_webhooks).post(create_webhook))
        .route("/v1/webhooks/{id}", delete(delete_webhook))
        .route("/v1/ai", get(get_ai))
        .route("/v1/ai/catalogue", get(ai_catalogue))
        .route("/v1/ai/providers", post(save_provider))
        .route("/v1/ai/providers/{id}", delete(remove_provider))
        .route("/v1/ai/providers/{id}/test", post(test_provider))
        .route("/v1/ai/discover", post(discover_providers))
        .route("/v1/ai/roles/{role}", post(bind_role))
        .route("/v1/ai/local-only", post(set_local_only))
        .route("/v1/ai/anthropic-key", post(save_anthropic_key))
        .route("/v1/admin/quiesce", post(quiesce))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            super::auth::require_auth,
        ));

    public.merge(private).with_state(state)
}

// ------------------------------------------------------------------ health --

async fn health_public() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

async fn health_detail(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let h = state.health().await.map_err(ApiError::from)?;
    Ok(Json(serde_json::to_value(h).unwrap_or(json!({}))))
}

// ------------------------------------------------------------------- tasks --

#[derive(Deserialize)]
struct ListTasksQuery {
    #[serde(default)]
    include_archived: bool,
}

async fn list_tasks(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Query(q): Query<ListTasksQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Read)?;
    let tasks = errand_core::db::list_tasks(state.pool(), q.include_archived)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "items": tasks })))
}

async fn get_task(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<errand_core::models::Task>> {
    require(&caller, Scope::Read)?;
    errand_core::db::get_task(state.pool(), &id)
        .await
        .map_err(ApiError::from)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("No task with id {id}.")))
}

#[derive(Deserialize)]
struct CreateTask {
    name: String,
    description: String,
    emoji: Option<String>,
    #[serde(default)]
    schedule: Option<serde_json::Value>,
}

async fn create_task(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Json(body): Json<CreateTask>,
) -> ApiResult<Json<errand_core::models::Task>> {
    require(&caller, Scope::Manage)?;
    if body.name.trim().is_empty() {
        return Err(ApiError::bad_request("A task needs a name."));
    }
    if body.description.trim().is_empty() {
        return Err(ApiError::bad_request(
            "A task needs a description. That description is what the agent actually reads, \
             so write it the way you would explain the job to a person.",
        ));
    }
    let schedule = body.schedule.unwrap_or_else(|| json!({ "kind": "manual" }));
    errand_core::schedule::ScheduleSpec::from_json(&schedule)
        .and_then(|s| s.validate())
        .map_err(|e| ApiError::bad_request(e.to_string()))?;

    let task = errand_core::db::create_task(
        state.pool(),
        errand_core::db::NewTask {
            name: body.name,
            description: body.description,
            emoji: body.emoji,
            schedule,
        },
    )
    .await
    .map_err(ApiError::from)?;

    state.emit(Event::TaskUpdated {
        task_id: task.id.clone(),
    });
    Ok(Json(task))
}

/// Move a task from draft to ready so the scheduler will consider it.
async fn activate_task(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Manage)?;
    let task = errand_core::db::get_task(state.pool(), &id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found(format!("No task with id {id}.")))?;

    // A schedule that cannot be read would leave the task armed but unable to
    // compute when it runs, which is worse than refusing here.
    let spec = errand_core::schedule::ScheduleSpec::from_json(&task.schedule)
        .and_then(|s| s.validate().map(|_| s))
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let next = spec
        .next_after(chrono::Utc::now())
        .map_err(|e| ApiError::bad_request(e.to_string()))?;

    if spec.is_scheduled()
        && errand_core::db::active_playbook(state.pool(), &id)
            .await
            .map_err(ApiError::from)?
            .is_none()
    {
        return Err(ApiError::conflict(
            "task_not_taught",
            "This task has no approved playbook, so putting it on a schedule would send an \
             unattended agent at a site with no agreed way of doing the job. Teach it once and \
             approve what it wrote first.",
        ));
    }

    if !errand_core::db::activate_task(state.pool(), &id)
        .await
        .map_err(ApiError::from)?
    {
        return Err(ApiError::conflict(
            "task_not_activatable",
            "Only a draft, teaching or ready task can be activated. An archived task cannot.",
        ));
    }

    if let Some(n) = next {
        let _ = errand_core::db::set_next_run_at(state.pool(), &id, &n.to_rfc3339()).await;
    }
    state.emit(Event::TaskUpdated { task_id: id });
    Ok(Json(json!({
        "status": "ready",
        "next_run_at": next.map(|n| n.to_rfc3339()),
        "note": if next.is_some() {
            "This task will now run on its own schedule."
        } else {
            "This task has no schedule, so it will only run when you ask it to."
        }
    })))
}

#[derive(Deserialize)]
struct ChannelConfig {
    /// Write-only. Goes to the keychain and is never returned.
    #[serde(default)]
    secrets: std::collections::HashMap<String, String>,
    /// Non-secret settings, such as where a gateway lives.
    #[serde(default)]
    settings: std::collections::HashMap<String, serde_json::Value>,
}

/// Set up a channel. Secrets go to the keychain; nothing sensitive is echoed.
async fn configure_channel(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(channel): Path<String>,
    Json(body): Json<ChannelConfig>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Manage)?;
    if crate::channels::ChannelId::parse(&channel).is_none() {
        return Err(ApiError::bad_request(format!(
            "'{channel}' is not a channel."
        )));
    }

    // Only the accounts a channel actually uses, so a typo cannot quietly
    // write a secret nothing will ever read.
    const ALLOWED: &[&str] = &[
        "telegram.bot_token",
        "telegram.chat_id",
        "telegram.owner_user_id",
        "whatsapp.api_key",
    ];

    let mut stored = vec![];
    for (k, v) in body.secrets {
        if !ALLOWED.contains(&k.as_str()) {
            return Err(ApiError::bad_request(format!(
                "'{k}' is not something Errand stores. Expected one of: {}",
                ALLOWED.join(", ")
            )));
        }
        crate::secrets::put_internal(&k, errand_core::keychain::Secret::new(v))
            .await
            .map_err(|e| {
                ApiError::internal(format!("Could not save that to your keychain: {e}"))
            })?;
        stored.push(k);
    }

    for (k, v) in body.settings {
        errand_core::db::set_setting(state.pool(), &k, &v)
            .await
            .map_err(ApiError::from)?;
    }

    let health = crate::channels::health_all(state.pool()).await;
    Ok(Json(json!({
        "stored": stored,
        "health": health.iter().find(|h| h.channel == channel),
        "note": "Secrets are in your macOS keychain. Errand can use them; it cannot show them \
                 back to you."
    })))
}

// ------------------------------------------------------------------ tokens --

async fn list_tokens(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Admin)?;
    let rows = errand_core::db::list_tokens(state.pool())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({
        "tokens": rows.iter().map(|(id, name, scopes, last)| json!({
            "id": id, "name": name, "scopes": scopes, "last_used_at": last
        })).collect::<Vec<_>>()
    })))
}

#[derive(Deserialize)]
struct MintToken {
    name: String,
    /// Comma separated, from: read, run, webhook, approve, manage, admin.
    scopes: String,
}

/// Mint a token for one client, with only the scopes it needs.
async fn mint_token(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Json(body): Json<MintToken>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Admin)?;
    if body.name.trim().is_empty() {
        return Err(ApiError::bad_request(
            "Give the token a name, so you can tell later which program is using it.",
        ));
    }

    let mut scopes = vec![];
    for raw in body.scopes.split(',') {
        let t = raw.trim();
        if t.is_empty() {
            continue;
        }
        match Scope::parse(t) {
            Some(s) => scopes.push(s),
            None => {
                return Err(ApiError::bad_request(format!(
                    "'{t}' is not a permission. Choose from: read, run, webhook, approve, \
                     manage, admin."
                )))
            }
        }
    }
    if scopes.is_empty() {
        return Err(ApiError::bad_request(
            "A token with no permissions can do nothing. Say what this client needs.",
        ));
    }

    let token = super::auth::generate_token().map_err(ApiError::from)?;
    let hash = super::auth::hash_token(&token);
    let csv = scopes
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(",");
    errand_core::db::insert_token(state.pool(), &body.name, &hash, &csv)
        .await
        .map_err(ApiError::from)?;

    Ok(Json(json!({
        "name": body.name,
        "scopes": csv,
        "token": token,
        "note": "This is the only time you will see it. Errand stores only a hash, so it cannot \
                 show it to you again. If you lose it, mint another and revoke this one."
    })))
}

async fn revoke_token(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Admin)?;
    let gone = errand_core::db::revoke_token(state.pool(), &id)
        .await
        .map_err(ApiError::from)?;
    if !gone {
        return Err(ApiError::not_found("No live token with that id."));
    }
    Ok(Json(
        json!({ "revoked": id, "note": "It stops working immediately." }),
    ))
}

// ---------------------------------------------------------------- webhooks --

async fn list_webhooks(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Read)?;
    let hooks = errand_core::db::list_webhooks(state.pool())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({
        "webhooks": hooks.iter().map(|h| json!({
            "id": h.id, "url": h.url, "events": h.events, "active": h.active,
            "failure_count": h.failure_count, "last_error": h.last_error,
            "created_at": h.created_at
        })).collect::<Vec<_>>()
    })))
}

#[derive(Deserialize)]
struct NewWebhook {
    url: String,
    events: Vec<String>,
}

async fn create_webhook(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Json(body): Json<NewWebhook>,
) -> ApiResult<Json<serde_json::Value>> {
    // Its own scope rather than manage, so a client can subscribe to its own
    // outcomes without also being able to rewrite playbooks.
    require(&caller, Scope::Webhook)?;

    if !errand_core::db::webhook_target_allowed(&body.url) {
        return Err(ApiError::bad_request(
            "A webhook may only point at your own machine or your own network. Errand calls these \
             addresses on a schedule, so allowing a public one would turn it into a way of making \
             your computer fetch whatever somebody chose.",
        ));
    }

    const KNOWN: &[&str] = &[
        "run.finished",
        "run.failed",
        "run.needs_attention",
        "task.updated",
    ];
    for e in &body.events {
        if !KNOWN.contains(&e.as_str()) {
            return Err(ApiError::bad_request(format!(
                "'{e}' is not an event Errand sends. Choose from: {}",
                KNOWN.join(", ")
            )));
        }
    }
    if body.events.is_empty() {
        return Err(ApiError::bad_request(
            "Say which events you want, or nothing will ever be delivered.",
        ));
    }

    let secret = super::auth::generate_token().map_err(ApiError::from)?;
    let id = errand_core::db::create_webhook(
        state.pool(),
        &caller.token_id,
        &body.url,
        &body.events,
        "keychain",
    )
    .await
    .map_err(ApiError::from)?;

    // The signing secret lives in the keychain, like every other secret.
    crate::secrets::put_internal(
        &format!("webhook.{id}"),
        errand_core::keychain::Secret::new(secret.clone()),
    )
    .await
    .map_err(|e| ApiError::internal(format!("Could not save the signing secret: {e}")))?;

    Ok(Json(json!({
        "id": id,
        "url": body.url,
        "events": body.events,
        "secret": secret,
        "note": "Keep the secret. Every delivery carries X-Errand-Signature, which is \
                 sha256=HMAC(secret, timestamp + '.' + body). Check it, and reject anything with a \
                 timestamp more than a few minutes old, so nobody can replay a delivery at you."
    })))
}

async fn delete_webhook(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Webhook)?;
    let gone = errand_core::db::delete_webhook(state.pool(), &id)
        .await
        .map_err(ApiError::from)?;
    if !gone {
        return Err(ApiError::not_found("No webhook with that id."));
    }
    let _ = crate::secrets::delete_internal(&format!("webhook.{id}")).await;
    Ok(Json(json!({ "deleted": id })))
}

/// Every channel, and what it would take to make each one work.
async fn list_channels(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Read)?;
    let health = crate::channels::health_all(state.pool()).await;
    Ok(Json(json!({
        "channels": health,
        "notes": {
            "telegram": "Where run outcomes go. This is the one Errand relies on.",
            "whatsapp": crate::channels::whatsapp::RISK_NOTICE,
            "apple_mail": "Needs macOS Automation permission, which Errand asks for when you \
                           press Enable, so the prompt appears while you are looking at it.",
            "imessage": "Needs Messages to be signed in, and the same Automation permission."
        }
    })))
}

/// Send a real message through the whole pipeline, so a green light means it
/// actually works rather than that the settings look plausible.
async fn test_channel(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(channel): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Manage)?;
    let Some(id) = crate::channels::ChannelId::parse(&channel) else {
        return Err(ApiError::bad_request(format!(
            "'{channel}' is not a channel."
        )));
    };

    // Only ever to you. A test that could message someone else would be a
    // convenient way to make Errand send anything to anyone.
    let recipient = match id {
        crate::channels::ChannelId::Telegram => {
            crate::channels::telegram::configured_chat_id().await
        }
        _ => None,
    };
    let Some(recipient) = recipient else {
        return Err(ApiError::conflict(
            "no_self_recipient",
            "A test only ever goes to you, and Errand does not know where that is for this \
             channel yet. Set your own address for it first.",
        ));
    };

    let queued = errand_core::db::enqueue_message(
        state.pool(),
        errand_core::db::NewMessage {
            run_id: None,
            task_id: None,
            class: "test".into(),
            channel: channel.clone(),
            recipient,
            recipient_label: Some("you".into()),
            subject: Some("Errand test".into()),
            body: format!(
                "This is a test from Errand, sent at {}. If you are reading it, this channel works.",
                errand_core::now_iso()
            ),
            is_failure: false,
        },
    )
    .await
    .map_err(ApiError::from)?;

    Ok(Json(json!({
        "queued": queued,
        "note": "Queued. It goes out within a few seconds; check the channel list afterwards to \
                 see whether it actually sent."
    })))
}

/// Ask macOS for Automation permission now, while somebody is watching.
async fn enable_channel(
    State(_state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(channel): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Manage)?;
    let Some(id) = crate::channels::ChannelId::parse(&channel) else {
        return Err(ApiError::bad_request(format!(
            "'{channel}' is not a channel."
        )));
    };
    // The prompt has to come from the daemon, because macOS grants Automation
    // to the process that sends the Apple Event. Asking from anywhere else
    // grants it to the wrong thing and the 03:00 run still fails.
    let health = crate::channels::apple::request_consent(id).await;
    Ok(Json(serde_json::to_value(health).unwrap_or(json!({}))))
}

/// Start the supervised first run of a task.
///
/// A teach run is how a task learns. It works from the description alone and
/// writes down what actually worked, which a person then approves.
async fn teach_task(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<errand_core::models::Run>> {
    require(&caller, Scope::Run)?;
    let task = errand_core::db::get_task(state.pool(), &id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found(format!("No task with id {id}.")))?;

    if let Some(busy) = errand_core::db::busy_run_for_task(state.pool(), &id)
        .await
        .map_err(ApiError::from)?
    {
        return Err(ApiError::conflict(
            "task_already_running",
            format!("This task is already running (run {busy})."),
        ));
    }

    let occurrence = format!("teach/{}", errand_core::new_id());
    let run = errand_core::db::try_create_run(
        state.pool(),
        &id,
        &occurrence,
        "teach",
        "teach",
        Some(&caller.token_name),
    )
    .await
    .map_err(|e| match e {
        errand_core::db::CreateRunError::AlreadyExists => {
            ApiError::conflict("occurrence_already_ran", "That teach run already exists.")
        }
        errand_core::db::CreateRunError::Other(e) => ApiError::from(e),
    })?;

    let _ = errand_core::db::set_task_status(state.pool(), &id, "teaching").await;
    errand_core::db::append_step(
        state.pool(),
        &run.id,
        "plan",
        &format!(
            "Teaching '{}': working it out from the description",
            task.name
        ),
        true,
        None,
    )
    .await
    .map_err(ApiError::from)?;

    state.emit(Event::RunStatus {
        run_id: run.id.clone(),
        task_id: id,
        status: errand_core::models::RunStatus::Queued,
    });
    tokio::spawn(crate::executor::run_to_completion(
        state.clone(),
        run.id.clone(),
    ));
    Ok(Json(run))
}

/// The playbook in force, and every version, so a person can read what their
/// agent believes about a site before trusting it.
async fn get_playbook(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Read)?;
    let versions = errand_core::db::list_playbook_versions(state.pool(), &id)
        .await
        .map_err(ApiError::from)?;
    let active = errand_core::db::active_playbook(state.pool(), &id)
        .await
        .map_err(ApiError::from)?;

    Ok(Json(json!({
        "active": active.as_ref().map(|p| json!({
            "version": p.version,
            "goal": p.goal,
            "markdown": p.to_markdown(),
        })),
        "versions": versions.iter().map(|v| json!({
            "version": v.version,
            "source": v.source,
            "approved": v.approved,
            "changelog": v.changelog,
            "created_by_run_id": v.created_by_run_id,
            "created_at": v.created_at,
            "sha256": v.sha256,
        })).collect::<Vec<_>>(),
        "note": if active.is_none() {
            "Nothing is approved yet, so scheduled runs will not start. Teach the task once, \
             read what it wrote, then approve it."
        } else {
            "Runs follow the approved version."
        }
    })))
}

/// Approve a version, which is the single gate between "the agent watched
/// itself once" and "the agent does this alone at 08:00".
async fn approve_playbook(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path((id, version)): Path<(String, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    // Approving decides what an unattended agent will do on a real site, so it
    // needs the same scope as answering an approval gate, not merely the one
    // that starts runs.
    require(&caller, Scope::Approve)?;

    let pb = errand_core::playbook::read(&id, version)
        .map_err(|e| ApiError::not_found(format!("Cannot read version {version}: {e}")))?;

    errand_core::db::set_active_playbook(state.pool(), &id, version)
        .await
        .map_err(ApiError::from)?;
    // A taught task becomes ready to be scheduled only once its playbook is
    // approved.
    let _ = errand_core::db::set_task_status(state.pool(), &id, "ready").await;

    state.emit(Event::TaskUpdated {
        task_id: id.clone(),
    });
    Ok(Json(json!({
        "approved": version,
        "steps": pb.steps.len(),
        "note": "This task will now follow that playbook. Activate it if you want it to run on a \
                 schedule."
    })))
}

#[derive(Deserialize)]
struct HoldsBody {
    /// What the person found when they checked the site.
    outcome: String,
    #[serde(default)]
    note: Option<String>,
}

/// Resolve an unresolved irreversible action so the task can run again.
///
/// A run that began a booking and died leaves the task blocked, which is
/// correct: nobody knows whether it went through. But without a way to say what
/// you found, the only way out is editing the database, so the safe state
/// becomes a permanently stuck one.
async fn resolve_holds(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Json(body): Json<HoldsBody>,
) -> ApiResult<Json<serde_json::Value>> {
    // Saying "this already happened" or "this did not happen" decides whether
    // a real booking gets made, so it needs the approval scope rather than the
    // one that merely starts runs.
    require(&caller, Scope::Approve)?;
    let note = body
        .note
        .unwrap_or_else(|| format!("resolved by {}", caller.token_name));

    let (n, msg) = match body.outcome.as_str() {
        "did_not_happen" => (
            errand_core::db::clear_holds(state.pool(), &id, &note)
                .await
                .map_err(ApiError::from)?,
            "Recorded that it did not happen. This task can run again.",
        ),
        "already_happened" => (
            errand_core::db::confirm_holds(state.pool(), &id, &note)
                .await
                .map_err(ApiError::from)?,
            "Recorded that it already happened. This slot will not be attempted again.",
        ),
        other => {
            return Err(ApiError::bad_request(format!(
                "'{other}' is not something I understand. Say either 'did_not_happen' or \
                 'already_happened', depending on what you found when you checked the site."
            )))
        }
    };

    if n > 0 {
        let _ = errand_core::db::set_task_paused(state.pool(), &id, false, None).await;
    }
    state.emit(Event::TaskUpdated { task_id: id });
    Ok(Json(json!({ "resolved": n, "note": msg })))
}

#[derive(Deserialize, Default)]
struct PauseBody {
    #[serde(default)]
    reason: Option<String>,
}

async fn pause_task(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    body: Option<Json<PauseBody>>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Run)?;
    let reason = body.and_then(|b| b.0.reason);
    let changed = errand_core::db::set_task_paused(state.pool(), &id, true, reason.as_deref())
        .await
        .map_err(ApiError::from)?;
    if !changed {
        return Err(ApiError::conflict(
            "task_not_pausable",
            "Only a ready or paused task can be paused. Drafts and archived tasks have no schedule to skip.",
        ));
    }
    state.emit(Event::TaskUpdated { task_id: id });
    Ok(Json(json!({
        "paused": true,
        "note": "Scheduled runs are skipped. Manual runs still work. Nothing is deleted, \
                 and missed occurrences are not replayed when you resume."
    })))
}

async fn resume_task(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Run)?;
    let changed = errand_core::db::set_task_paused(state.pool(), &id, false, None)
        .await
        .map_err(ApiError::from)?;
    if !changed {
        return Err(ApiError::conflict(
            "task_not_resumable",
            "That task is not paused.",
        ));
    }
    state.emit(Event::TaskUpdated { task_id: id });
    Ok(Json(json!({ "paused": false })))
}

#[derive(Deserialize, Default)]
struct RunBody {
    #[serde(default)]
    dry_run: bool,
}

async fn run_task(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    body: Option<Json<RunBody>>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Run)?;

    // A client that retries after a dropped connection must get the same run
    // back rather than a second booking. This is the difference between a
    // network blip and a duplicate court.
    let idem = headers
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s.len() <= 128);

    let request_hash = super::auth::hash_token(&format!(
        "{id}:{}",
        body.as_ref().map(|b| b.0.dry_run).unwrap_or(false)
    ));

    if let Some(key) = &idem {
        match errand_core::db::idempotent_replay(state.pool(), key, "run", &request_hash)
            .await
            .map_err(ApiError::from)?
        {
            Some(Ok(stored)) => {
                let v: serde_json::Value =
                    serde_json::from_str(&stored).unwrap_or(serde_json::Value::Null);
                return Ok(Json(v));
            }
            Some(Err(why)) => {
                return Err(ApiError::conflict("idempotency_key_reuse", why));
            }
            None => {}
        }
    }
    if state.is_quiescing() {
        return Err(ApiError::conflict(
            "quiescing",
            "Errand is shutting down for an update and is not starting new runs.",
        ));
    }
    let task = errand_core::db::get_task(state.pool(), &id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found(format!("No task with id {id}.")))?;

    // The single gate between "the agent tried this once" and "the agent does
    // this alone". A task with no approved playbook may only be taught.
    if errand_core::db::active_playbook(state.pool(), &id)
        .await
        .map_err(ApiError::from)?
        .is_none()
    {
        return Err(ApiError::conflict(
            "task_not_taught",
            "This task has no approved playbook, so there is nothing to follow yet. Teach it \
             once with POST /v1/tasks/{id}/teach, read what it wrote down, and approve that \
             before it runs on its own.",
        ));
    }

    // Refuse to start a second agent on a task that is already running one.
    // Two agents on the same task can each book the same slot, and the unique
    // index does not catch it because their occurrence ids differ.
    if let Some(busy) = errand_core::db::busy_run_for_task(state.pool(), &id)
        .await
        .map_err(ApiError::from)?
    {
        return Err(ApiError::conflict(
            "task_already_running",
            format!(
                "This task is already running (run {busy}). Wait for it to finish, or cancel it, \
                 rather than starting a second one alongside it."
            ),
        ));
    }

    let dry = body.map(|b| b.0.dry_run).unwrap_or(false);

    // Give a manual run the identity of the slot it stands in for.
    //
    // A fresh id per press would mean the fence never recognises the work the
    // scheduled run already did, so the ordinary sequence of a run dying
    // mid-booking and the user pressing Run now would book a second court with
    // nothing to stop it.
    let spec = errand_core::schedule::ScheduleSpec::from_json(&task.schedule)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let occurrence = match spec
        .last_occurrence_at_or_before(chrono::Utc::now())
        .map_err(|e| ApiError::bad_request(e.to_string()))?
    {
        Some(occ) => spec.occurrence_id(occ),
        None => format!("manual/{}", errand_core::new_id()),
    };

    // An unresolved irreversible action means nobody knows what already
    // happened. Starting another run could repeat it.
    if errand_core::db::dangling_fences(state.pool(), &id, &occurrence)
        .await
        .map_err(ApiError::from)?
    {
        return Err(ApiError::conflict(
            "needs_verification",
            "An earlier attempt at this slot began something that cannot be undone and never \
             confirmed whether it finished. Running again could do it twice. Check the site \
             first, then clear this from the task before running it again.",
        ));
    }

    let run = match errand_core::db::try_create_run(
        state.pool(),
        &id,
        &occurrence,
        "api",
        if dry { "dry_run" } else { "normal" },
        Some(&caller.token_name),
    )
    .await
    {
        Ok(r) => r,
        Err(errand_core::db::CreateRunError::AlreadyExists) => {
            return Err(ApiError::conflict(
                "occurrence_already_ran",
                "This task has already run for the current scheduled slot. Running again now \
                 would repeat work that was already done. Wait for the next slot, or look at \
                 what the last run did.",
            ))
        }
        Err(errand_core::db::CreateRunError::Other(e)) => return Err(ApiError::from(e)),
    };

    errand_core::db::append_step(
        state.pool(),
        &run.id,
        "plan",
        &format!("Run requested via API by '{}'", caller.token_name),
        true,
        None,
    )
    .await
    .map_err(ApiError::from)?;

    state.emit(Event::RunStatus {
        run_id: run.id.clone(),
        task_id: id,
        status: errand_core::models::RunStatus::Queued,
    });

    // Hand the run to the contained executor and return immediately. Callers
    // follow progress on the SSE stream rather than holding a request open.
    tokio::spawn(crate::executor::run_to_completion(
        state.clone(),
        run.id.clone(),
    ));

    let body = serde_json::to_value(&run).unwrap_or(json!({}));
    if let Some(key) = idem {
        let _ = errand_core::db::remember_idempotent(
            state.pool(),
            &key,
            "run",
            &request_hash,
            202,
            &body.to_string(),
        )
        .await;
    }
    Ok(Json(body))
}

// -------------------------------------------------------------------- runs --

#[derive(Deserialize)]
struct ListRunsQuery {
    task_id: Option<String>,
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    50
}

async fn list_runs(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Query(q): Query<ListRunsQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Read)?;
    let runs =
        errand_core::db::list_runs(state.pool(), q.task_id.as_deref(), q.limit.clamp(1, 500))
            .await
            .map_err(ApiError::from)?;
    Ok(Json(json!({ "items": runs })))
}

async fn get_run(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Read)?;
    let run = errand_core::db::get_run(state.pool(), &id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found(format!("No run with id {id}.")))?;
    let steps = errand_core::db::list_steps(state.pool(), &id)
        .await
        .map_err(ApiError::from)?;
    let mut body = serde_json::to_value(&run).unwrap_or(json!({}));
    body["steps"] = serde_json::to_value(steps).unwrap_or(json!([]));
    Ok(Json(body))
}

// ------------------------------------------------------------- credentials --

async fn list_credentials(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Manage)?;
    let mut creds = errand_core::db::list_credentials(state.pool())
        .await
        .map_err(ApiError::from)?;
    // Usernames are only for admin tokens: below that they are a map of which
    // accounts exist on which sites.
    if !caller.has(Scope::Admin) {
        for c in &mut creds {
            c.username = None;
        }
    }
    Ok(Json(json!({ "items": creds })))
}

#[derive(Deserialize)]
struct CreateCredential {
    label: String,
    domain: String,
    #[serde(default = "default_kind")]
    kind: String,
    username: Option<String>,
    /// Write-only. Goes straight to the keychain and is never echoed back.
    secret: String,
}

fn default_kind() -> String {
    "password".into()
}

async fn create_credential(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Json(body): Json<CreateCredential>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Manage)?;
    if body.secret.is_empty() {
        return Err(ApiError::bad_request("The secret cannot be empty."));
    }
    if body.domain.trim().is_empty() {
        return Err(ApiError::bad_request(
            "A credential must be bound to a domain. The agent will refuse to type it into \
             any other site, which is what stops a lookalike page from collecting it.",
        ));
    }

    let id = errand_core::db::create_credential_meta(
        state.pool(),
        &body.label,
        &body.kind,
        &body.domain,
        body.username.as_deref(),
    )
    .await
    .map_err(ApiError::from)?;

    let secret = errand_core::keychain::Secret::new(body.secret);
    if let Err(e) = crate::secrets::put(
        errand_core::keychain_service(),
        format!("cred/{id}/v1"),
        secret,
    )
    .await
    {
        // Do not leave metadata pointing at a keychain item that does not exist.
        let _ = errand_core::db::delete_credential_meta(state.pool(), &id).await;
        return Err(ApiError::internal(format!(
            "Could not save that credential to your keychain: {e}"
        )));
    }

    Ok(Json(json!({
        "id": id,
        "stored": "keychain",
        "note": "The secret is in your macOS keychain. Errand can use it; it cannot show it to you."
    })))
}

async fn delete_credential(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Manage)?;
    let pair = errand_core::db::delete_credential_meta(state.pool(), &id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found(format!("No credential with id {id}.")))?;

    let _ = crate::secrets::delete(pair.0, pair.1).await;
    Ok(Json(json!({ "deleted": id })))
}

// ------------------------------------------------------------------- admin --

/// Drain and exit cleanly so the updater can swap the bundle underneath us.
///
/// The clean `exit(0)` matters: with `KeepAlive: {SuccessfulExit: false}`,
/// launchd leaves a cleanly exited daemon down instead of restarting it from a
/// half-replaced bundle. The installer then kickstarts the new binary, which
/// kills any survivor rather than racing it for the lock.
async fn quiesce(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Admin)?;
    state.begin_quiesce();
    let busy = errand_core::db::count_busy_runs(state.pool())
        .await
        .unwrap_or(0);

    tokio::spawn(async move {
        // Give in-flight requests a moment to finish writing.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        tracing::info!("quiesced on request; exiting cleanly for update");
        std::process::exit(0);
    });

    Ok(Json(json!({
        "quiescing": true,
        "busy_runs": busy,
        "note": "Exiting cleanly. launchd will leave the runner down until it is kickstarted."
    })))
}

// ---------------------------------------------------------------------- ai --
//
// Which AI does the work is a question the app has to be able to answer out
// loud, so all of it — what is configured, what is reachable, and what each job
// would actually use right now — comes back from one call.

async fn get_ai(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Read)?;
    let providers = errand_core::db::list_providers(state.pool())
        .await
        .map_err(ApiError::from)?;
    let bindings = errand_core::db::list_role_bindings(state.pool())
        .await
        .map_err(ApiError::from)?;
    let local_only = local_only_setting(&state).await;

    // For each job, what would happen if it were asked right now. This is the
    // part that turns "it uses AI somehow" into something you can point at.
    let roles: Vec<serde_json::Value> = errand_core::providers::Role::ALL
        .iter()
        .map(|&role| {
            let chain =
                errand_core::providers::resolve_chain(&providers, &bindings, role, local_only);
            let chosen = bindings
                .iter()
                .find(|(r, _)| *r == role)
                .map(|(_, id)| id.clone());
            json!({
                "role": role.as_str(),
                "explains": role.describe(),
                "needs_agentic": role.needs_agentic(),
                // Said plainly, so the screen cannot offer a choice that would
                // not change anything.
                "in_use": role.is_wired(),
                "not_used_because": role.not_wired_reason(),
                "chosen": chosen,
                "using": chain.first().map(|p| json!({
                    "id": p.id,
                    "label": p.label,
                    "model": p.model.clone().unwrap_or_else(||
                        errand_core::providers::default_model_for(role).to_string()),
                    "local": p.is_local(),
                })),
                "fallbacks": chain.iter().skip(1).map(|p| p.label.clone()).collect::<Vec<_>>(),
                "problem": chain.is_empty().then(||
                    errand_core::providers::explain_empty_chain(role, local_only, &providers)),
            })
        })
        .collect();

    Ok(Json(json!({
        "providers": providers,
        "roles": roles,
        "local_only": local_only,
    })))
}

async fn local_only_setting(state: &AppState) -> bool {
    errand_core::db::get_setting(state.pool(), "privacy.local_only")
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Every service Errand knows by name, so nobody has to look up a base URL.
async fn ai_catalogue(Extension(caller): Extension<Caller>) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Read)?;
    Ok(Json(
        json!({ "services": errand_core::providers::CATALOGUE }),
    ))
}

#[derive(Deserialize)]
struct SaveProvider {
    id: Option<String>,
    /// One of the names Errand knows, e.g. "openai". Fills in everything else.
    known: Option<String>,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    label: String,
    base_url: Option<String>,
    model: Option<String>,
    /// Write-only. Goes to the keychain and is never read back out.
    key: Option<String>,
    #[serde(default = "yes")]
    enabled: bool,
}

fn yes() -> bool {
    true
}

async fn save_provider(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Json(body): Json<SaveProvider>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Manage)?;

    // Picking a service by name fills in the address, so a base URL is
    // something you only ever type for something Errand has not heard of.
    let known = match &body.known {
        Some(id) => Some(errand_core::providers::known(id).ok_or_else(|| {
            ApiError::bad_request(format!("Errand does not know a service called '{id}'."))
        })?),
        None => None,
    };

    let kind_str = if body.kind.is_empty() && known.is_some() {
        "openai_compat"
    } else {
        body.kind.as_str()
    };
    let kind = errand_core::providers::Kind::parse(kind_str).ok_or_else(|| {
        ApiError::bad_request(format!(
            "'{kind_str}' is not a kind of AI Errand knows about."
        ))
    })?;

    // Checked before it is saved, so a typo is a message now rather than a
    // failed run at seven in the morning.
    let base_url = match kind {
        errand_core::providers::Kind::OpenAiCompat => {
            let raw = body
                .base_url
                .clone()
                .filter(|u| !u.trim().is_empty())
                .or_else(|| known.map(|k| k.base_url.to_string()))
                .unwrap_or_default();
            if raw.trim().is_empty() {
                return Err(ApiError::bad_request(
                    "A model of your own needs an address, such as http://127.0.0.1:11434",
                ));
            }
            Some(
                errand_core::providers::parse_base_url(&raw)
                    .map_err(|e| ApiError::bad_request(e.to_string()))?,
            )
        }
        _ => None,
    };

    let provider = errand_core::providers::Provider {
        // A known service keeps its own id, so adding OpenAI twice updates the
        // one row rather than quietly stacking up duplicates with one key each.
        id: body
            .id
            .or_else(|| known.map(|k| k.id.to_string()))
            .unwrap_or_else(errand_core::new_id),
        kind: kind.as_str().into(),
        label: if !body.label.trim().is_empty() {
            body.label.trim().to_string()
        } else if let Some(k) = known {
            k.name.to_string()
        } else {
            kind.as_str().to_string()
        },
        base_url,
        model: body.model.filter(|m| !m.trim().is_empty()).or_else(|| {
            known
                .map(|k| k.example_model.to_string())
                .filter(|m| !m.is_empty())
        }),
        enabled: body.enabled,
        discovered: false,
        health: None,
        health_detail: None,
    };

    // The key goes to the keychain before the row is written, so a saved
    // provider is never left pointing at a key that failed to store.
    if let Some(raw) = body.key.as_ref() {
        let key = raw.trim();
        if key.is_empty() {
            crate::secrets::delete_internal(&errand_core::providers::key_account(&provider.id))
                .await
                .ok();
        } else {
            if let Some(k) = known {
                if !k.key_prefix.is_empty() && !key.starts_with(k.key_prefix) {
                    return Err(ApiError::bad_request(format!(
                        "A {} key starts with {}. Check you pasted the right one.",
                        k.name, k.key_prefix
                    )));
                }
            }
            crate::secrets::put_internal(
                &errand_core::providers::key_account(&provider.id),
                errand_core::keychain::Secret::new(key.to_string()),
            )
            .await
            .map_err(|_| {
                ApiError::internal(
                    "Errand could not write to your keychain, so the key was not saved. If macOS \
                     asked for permission and it was refused, allow it and try again.",
                )
            })?;
        }
    }

    errand_core::db::upsert_provider(state.pool(), &provider)
        .await
        .map_err(ApiError::from)?;

    let (status, detail) = crate::models::check_one(&provider).await;
    errand_core::db::set_provider_health(state.pool(), &provider.id, status, Some(&detail))
        .await
        .map_err(ApiError::from)?;

    Ok(Json(json!({
        "id": provider.id,
        "health": status,
        "health_detail": detail,
    })))
}

async fn remove_provider(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Manage)?;
    if id == crate::models::BUILTIN_CLAUDE {
        return Err(ApiError::bad_request(
            "This is the AI Errand falls back to. You can switch it off, but removing it would \
             leave nothing to run tasks with.",
        ));
    }
    let gone = errand_core::db::delete_provider(state.pool(), &id)
        .await
        .map_err(ApiError::from)?;
    if !gone {
        return Err(ApiError::not_found("There is no such model in the list."));
    }
    // The key goes with it. Leaving a key in the keychain for something the
    // person believes they removed is exactly the surprise to avoid.
    crate::secrets::delete_internal(&errand_core::providers::key_account(&id))
        .await
        .ok();
    Ok(Json(json!({ "removed": true })))
}

/// Ask a provider whether it is really there, and say what came back.
async fn test_provider(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Manage)?;
    let providers = errand_core::db::list_providers(state.pool())
        .await
        .map_err(ApiError::from)?;
    let p = providers
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| ApiError::not_found("There is no such model in the list."))?;

    let (status, detail) = crate::models::check_one(&p).await;
    errand_core::db::set_provider_health(state.pool(), &id, status, Some(&detail))
        .await
        .map_err(ApiError::from)?;

    Ok(Json(json!({ "health": status, "health_detail": detail })))
}

#[derive(Deserialize)]
struct Discover {
    /// Look on the local network too, not just this machine. Off by default,
    /// because scanning somebody's network is not something to do unasked.
    #[serde(default)]
    scan_network: bool,
}

fn scanned_where(scan_network: bool) -> &'static str {
    if scan_network {
        "this machine and every address on your network"
    } else {
        "this machine only"
    }
}

/// Look for models running on this machine, and optionally on this network.
///
/// The flag is a query parameter rather than a body, so that "did it scan?" can
/// never come down to whether a body parsed. An optional body silently becomes
/// "no" when anything about the request is slightly off, and a scan that
/// quietly did not happen looks exactly like a network with nothing on it.
async fn discover_providers(
    Extension(caller): Extension<Caller>,
    Query(q): Query<Discover>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Manage)?;
    let found = crate::models::discover(q.scan_network).await;
    Ok(Json(json!({
        "found": found,
        // Said back, so nobody has to infer from an empty list whether the scan
        // they asked for actually happened.
        "looked_at": scanned_where(q.scan_network),
        // Nothing is switched on by this call. Finding something and using it
        // are separate decisions, and the second one is the person's.
        "note": "Nothing has been added yet. Pick the ones you want to use.",
    })))
}

#[derive(Deserialize)]
struct BindRole {
    /// None means "no preference": use whatever is available.
    provider_id: Option<String>,
}

async fn bind_role(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(role): Path<String>,
    Json(body): Json<BindRole>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Manage)?;
    let role = errand_core::providers::Role::parse(&role)
        .ok_or_else(|| ApiError::bad_request(format!("'{role}' is not one of Errand's jobs.")))?;

    // Refused with a reason rather than accepted and quietly ignored later.
    if let Some(id) = &body.provider_id {
        let providers = errand_core::db::list_providers(state.pool())
            .await
            .map_err(ApiError::from)?;
        let p = providers
            .iter()
            .find(|p| &p.id == id)
            .ok_or_else(|| ApiError::not_found("There is no such model in the list."))?;
        if role.needs_agentic() && !p.can_carry_out_tasks() {
            return Err(ApiError::bad_request(
                p.cannot_fill(role).unwrap_or_default(),
            ));
        }
    }

    errand_core::db::set_role_binding(state.pool(), role, body.provider_id.as_deref())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct LocalOnly {
    enabled: bool,
}

async fn set_local_only(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Json(body): Json<LocalOnly>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Manage)?;

    // Turning this on with nothing local configured would silently stop every
    // task, so it is refused with the reason instead.
    if body.enabled {
        let providers = errand_core::db::list_providers(state.pool())
            .await
            .map_err(ApiError::from)?;
        if !providers.iter().any(|p| p.enabled && p.is_local()) {
            return Err(ApiError::bad_request(
                "There is no model on this machine for Errand to use, so turning this on would \
                 stop everything. Add one first — Find models on this machine will look.",
            ));
        }
    }

    errand_core::db::set_setting(state.pool(), "privacy.local_only", &json!(body.enabled))
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "local_only": body.enabled })))
}

#[derive(Deserialize)]
struct AnthropicKey {
    /// Write-only. Goes to the keychain and is never read back out to anyone.
    key: String,
}

async fn save_anthropic_key(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Json(body): Json<AnthropicKey>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Manage)?;
    let key = body.key.trim().to_string();
    if key.is_empty() {
        crate::secrets::delete_internal("anthropic.api_key")
            .await
            .ok();
        return Ok(Json(json!({ "saved": false, "removed": true })));
    }
    if !key.starts_with("sk-ant-") {
        return Err(ApiError::bad_request(
            "An Anthropic key starts with sk-ant-. Check you pasted the whole thing.",
        ));
    }

    crate::secrets::put_internal("anthropic.api_key", errand_core::keychain::Secret::new(key))
        .await
        .map_err(|_| {
            ApiError::internal(
                "Errand could not write to your keychain. If macOS asked for permission and it \
                 was refused, allow it and try again.",
            )
        })?;

    // Having a key is only useful with something to use it, so the row appears
    // at the same moment rather than needing a second step nobody would guess.
    let provider = errand_core::providers::Provider {
        id: "anthropic-api".into(),
        kind: errand_core::providers::Kind::AnthropicApi.as_str().into(),
        label: "Anthropic API".into(),
        base_url: None,
        model: None,
        enabled: true,
        discovered: false,
        health: Some("ok".into()),
        health_detail: Some("a key is saved in your keychain".into()),
    };
    errand_core::db::upsert_provider(state.pool(), &provider)
        .await
        .map_err(ApiError::from)?;

    Ok(Json(json!({ "saved": true, "provider_id": provider.id })))
}

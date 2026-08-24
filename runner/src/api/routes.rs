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
    body: Option<Json<RunBody>>,
) -> ApiResult<Json<errand_core::models::Run>> {
    require(&caller, Scope::Run)?;
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

    Ok(Json(run))
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

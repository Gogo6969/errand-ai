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
        .route("/v1/tasks/{id}", get(get_task).patch(patch_task))
        .route("/v1/schedule/preview", post(preview_schedule))
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
        .route(
            "/v1/recipients",
            get(list_recipients).post(create_recipient),
        )
        .route("/v1/recipients/{id}", delete(delete_recipient))
        .route(
            "/v1/tasks/{id}/recipients",
            get(list_task_recipients).post(grant_recipient),
        )
        .route(
            "/v1/tasks/{id}/recipients/{recipient_id}",
            delete(revoke_recipient),
        )
        .route("/v1/settings", get(get_settings))
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

/// How many upcoming runs to show alongside a schedule.
///
/// Enough to see the pattern — three mornings in a row, or the same day next
/// week — without turning a task page into a diary.
const PREVIEW_COUNT: usize = 3;

/// Said in one place, because two gates enforce this one rule.
///
/// `activate` refuses an untaught task that is already on a schedule; `patch`
/// refuses putting an untaught task onto one. If the two ever explained it
/// differently, a person meeting the second would think they had hit a
/// different problem.
const NOT_TAUGHT: &str =
    "This task has no approved playbook, so putting it on a schedule would send an unattended \
     agent at a site with no agreed way of doing the job. Teach it once and approve what it \
     wrote first.";

/// A task, plus its schedule said in words and the next few times it will run.
///
/// The interface must never have to read a cron expression to work out what a
/// task does; a screen that interprets one for itself is a screen that can
/// disagree with the engine, and the disagreement only shows up on the morning
/// nothing happens. Both of these come from the code that will actually fire
/// the task, so they cannot drift from it.
fn task_json(task: &errand_core::models::Task) -> serde_json::Value {
    let mut v = serde_json::to_value(task).unwrap_or_else(|_| json!({}));
    let (describes, preview) = match errand_core::schedule::ScheduleSpec::from_json(&task.schedule)
    {
        Ok(spec) => (
            spec.describe(),
            spec.preview(chrono::Utc::now(), PREVIEW_COUNT)
                .unwrap_or_default()
                .iter()
                // The moment each run begins, the same expression the countdown
                // and the sweep use. Listing the bare occurrences instead would
                // put two different times for the same run on one screen as soon
                // as a task had any jitter or a window it arms early for.
                .map(|d| crate::scheduler::start_instant(task, &spec, *d).to_rfc3339())
                .collect::<Vec<_>>(),
        ),
        // Saying so beats leaving the field out: a task whose schedule cannot
        // be read will never run on its own, and nobody would guess that from
        // a missing line.
        Err(e) => (
            format!("Errand cannot read this task's schedule, so it will not run on its own: {e}"),
            vec![],
        ),
    };
    v["schedule_describes"] = json!(describes);
    v["schedule_preview"] = json!(preview);
    v
}

/// Tidy a list of sites, or refuse it with something to type instead.
///
/// Warnings come back alongside: they are not refusals, and the list they
/// describe has been accepted.
fn normalize_sites(list: &[String]) -> ApiResult<(Vec<String>, Vec<String>)> {
    let n = errand_core::domains::normalize_domains(list)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok((n.domains, n.warnings))
}

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
    let items: Vec<serde_json::Value> = tasks.iter().map(task_json).collect();
    Ok(Json(json!({ "items": items })))
}

async fn get_task(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Read)?;
    errand_core::db::get_task(state.pool(), &id)
        .await
        .map_err(ApiError::from)?
        .map(|t| Json(task_json(&t)))
        .ok_or_else(|| ApiError::not_found(format!("No task with id {id}.")))
}

#[derive(Deserialize)]
struct CreateTask {
    name: String,
    description: String,
    emoji: Option<String>,
    #[serde(default)]
    schedule: Option<serde_json::Value>,
    #[serde(default)]
    notify: Option<serde_json::Value>,
    #[serde(default)]
    limits: Option<serde_json::Value>,
    #[serde(default)]
    allowed_domains: Option<Vec<String>>,
}

async fn create_task(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Json(body): Json<CreateTask>,
) -> ApiResult<Json<serde_json::Value>> {
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

    // Checked before anything is written, so a site entry that cannot work
    // costs nothing and leaves no half-made task behind.
    let (domains, warnings) = match &body.allowed_domains {
        Some(list) => {
            let (d, w) = normalize_sites(list)?;
            (Some(d), w)
        }
        None => (None, vec![]),
    };

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

    // The rest goes through the same code that a later edit would use, rather
    // than a second copy of it here. One implementation of "store these
    // settings" is one thing to get wrong.
    let task = if domains.is_some() || body.notify.is_some() || body.limits.is_some() {
        errand_core::db::update_task(
            state.pool(),
            &task.id,
            errand_core::db::TaskPatch {
                notify: body.notify,
                limits: body.limits,
                allowed_domains: domains,
                ..Default::default()
            },
        )
        .await
        .map_err(ApiError::from)?
    } else {
        task
    };

    state.emit(Event::TaskUpdated {
        task_id: task.id.clone(),
    });
    let mut out = task_json(&task);
    out["warnings"] = json!(warnings);
    Ok(Json(out))
}

/// Everything about a task a person may change after it exists.
///
/// Every field is optional and absent means unchanged, so a screen that edits
/// only the sites cannot blank the description on the way past.
#[derive(Deserialize)]
struct PatchTask {
    name: Option<String>,
    emoji: Option<String>,
    description: Option<String>,
    schedule: Option<serde_json::Value>,
    notify: Option<serde_json::Value>,
    limits: Option<serde_json::Value>,
    allowed_domains: Option<Vec<String>>,
    /// The answer to a `schedule_change_may_repeat` refusal: yes, I have read
    /// it, change the schedule anyway.
    #[serde(default)]
    acknowledge_repeat: bool,
}

/// Change a task's settings.
///
/// Nothing here touches a run that is already going, and nothing here unpauses
/// a task: somebody editing a schedule has not necessarily looked at the site.
async fn patch_task(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    Json(raw): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Manage)?;

    let body: PatchTask = serde_json::from_value(raw.clone()).map_err(|e| {
        ApiError::bad_request(format!(
            "Errand could not read that change, so nothing was altered: {e}"
        ))
    })?;

    // A save that is retried after a dropped connection must not be treated as
    // a second edit. Changing a schedule moves the task's catch-up floor, and
    // two floors written a moment apart is a task whose history has a hole in
    // it that nobody asked for.
    let idem = headers
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s.len() <= 128);
    let request_hash = super::auth::hash_token(&format!("{id}:{raw}"));
    if let Some(key) = &idem {
        match errand_core::db::idempotent_replay(state.pool(), key, "patch_task", &request_hash)
            .await
            .map_err(ApiError::from)?
        {
            Some(Ok(stored)) => {
                let v: serde_json::Value =
                    serde_json::from_str(&stored).unwrap_or(serde_json::Value::Null);
                return Ok(Json(v));
            }
            Some(Err(why)) => return Err(ApiError::conflict("idempotency_key_reuse", why)),
            None => {}
        }
    }

    let task = errand_core::db::get_task(state.pool(), &id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found(format!("No task with id {id}.")))?;

    if task.status == "archived" {
        return Err(ApiError::conflict(
            "task_archived",
            "This task has been archived, so its settings can no longer be changed. Nothing was \
             altered. Restore it first if you want to edit it.",
        ));
    }

    // Present-but-empty is a different thing from absent, and the create route
    // refuses both of these. Without the same check here, editing would be the
    // way round it.
    if body.name.as_ref().is_some_and(|n| n.trim().is_empty()) {
        return Err(ApiError::bad_request(
            "A task needs a name, so nothing was changed. Type one, or leave the name out of the \
             change to keep the one it has.",
        ));
    }
    if body
        .description
        .as_ref()
        .is_some_and(|d| d.trim().is_empty())
    {
        return Err(ApiError::bad_request(
            "A task needs a description — it is what the agent actually reads — so nothing was \
             changed. Leave the description out of the change to keep the one it has.",
        ));
    }

    let mut warnings: Vec<String> = vec![];
    let domains = match &body.allowed_domains {
        Some(list) => {
            let (d, w) = normalize_sites(list)?;
            warnings.extend(w);
            Some(d)
        }
        None => None,
    };

    let new_spec = match &body.schedule {
        Some(v) => Some(
            errand_core::schedule::ScheduleSpec::from_json(v)
                .and_then(|s| s.validate().map(|_| s))
                .map_err(|e| ApiError::bad_request(e.to_string()))?,
        ),
        None => None,
    };

    if let Some(spec) = &new_spec {
        // The hole this closes: `activate` only asks for a playbook when the
        // task is ALREADY on a schedule, so a manual task activates happily and
        // could then be moved onto a cron afterwards. That would put an
        // unattended agent on a real site with no approved plan, which is the
        // one thing that gate exists to prevent.
        if spec.is_scheduled()
            && matches!(task.status.as_str(), "ready" | "paused")
            && errand_core::db::active_playbook(state.pool(), &id)
                .await
                .map_err(ApiError::from)?
                .is_none()
        {
            return Err(ApiError::conflict("task_not_taught", NOT_TAUGHT));
        }

        if !body.acknowledge_repeat {
            if let Some(problem) = repeat_risk(&state, &task, spec).await? {
                return Err(ApiError::conflict("schedule_change_may_repeat", problem));
            }
        }
    }

    // A run already going is left alone rather than interrupted. It keeps the
    // slot it started in, so the guard that stops an action happening twice
    // still recognises its work; a run cut off midway through a booking is the
    // state nobody can untangle afterwards.
    if let Some(busy) = errand_core::db::busy_run_for_task(state.pool(), &id)
        .await
        .map_err(ApiError::from)?
    {
        warnings.push(format!(
            "This task is running right now (run {busy}). That run carries on to the end under \
             the settings it started with, and it is not started again. The change takes effect \
             from the next run onwards."
        ));
    }

    let updated = errand_core::db::update_task(
        state.pool(),
        &id,
        errand_core::db::TaskPatch {
            name: body.name,
            emoji: body.emoji,
            description: body.description,
            schedule: body.schedule,
            notify: body.notify,
            limits: body.limits,
            allowed_domains: domains,
        },
    )
    .await
    .map_err(ApiError::from)?;

    state.emit(Event::TaskUpdated {
        task_id: id.clone(),
    });

    let out = json!({ "task": task_json(&updated), "warnings": warnings });
    if let Some(key) = idem {
        let _ = errand_core::db::remember_idempotent(
            state.pool(),
            &key,
            "patch_task",
            &request_hash,
            200,
            &out.to_string(),
        )
        .await;
    }
    Ok(Json(out))
}

/// The kinds of irreversible action the guard records, as `browser::classify`
/// names them. Kept in step with that list by hand, because the guard writes
/// these strings and nothing else ever reads them back.
const ACTION_KINDS: &[&str] = &["purchase", "deletion", "booking", "message", "form_submit"];

/// How far back to look for finished work when a task has no repeating slot to
/// measure from. A day: long enough to catch this morning's booking, short
/// enough not to warn about something from last month.
const LOOKBACK_WITHOUT_A_SLOT_MIN: i64 = 24 * 60;

/// Would this schedule change make the task do something a second time?
///
/// Errand recognises work it has already finished by the exact instant the run
/// was due. Move a schedule forwards and the next run stands in a slot that has
/// never been seen before, so a booking made an hour ago counts for nothing and
/// gets made again. That is a decision for the person rather than something to
/// paper over, so it is refused once, in words, and can then be confirmed.
async fn repeat_risk(
    state: &AppState,
    task: &errand_core::models::Task,
    new_spec: &errand_core::schedule::ScheduleSpec,
) -> ApiResult<Option<String>> {
    let now = chrono::Utc::now();
    // An unreadable old schedule was never going to come round on its own, so
    // treat it as the manual case rather than guessing at it.
    let old_spec = errand_core::schedule::ScheduleSpec::from_json(&task.schedule)
        .unwrap_or_else(|_| errand_core::schedule::ScheduleSpec::default());

    let Some(new_first) = new_spec
        .next_after(now)
        .map_err(|e| ApiError::bad_request(e.to_string()))?
    else {
        return Ok(None);
    };
    let sooner = match old_spec.next_after(now).ok().flatten() {
        Some(old_next) => new_first < old_next,
        // A task that was never coming round again on its own now is, so
        // anything the new schedule does is sooner than nothing.
        None => true,
    };
    if !sooner {
        return Ok(None);
    }

    // Only work belonging to the slot the task is in now could be repeated:
    // anything older was already superseded by a later occurrence.
    let minutes = old_spec
        .last_occurrence_at_or_before(now)
        .ok()
        .flatten()
        .map(|occ| (now - occ).num_minutes().max(1))
        .unwrap_or(LOOKBACK_WITHOUT_A_SLOT_MIN);

    for kind in ACTION_KINDS {
        // Whoever it was aimed at. Asking the scoped question here would be
        // asking the wrong one: every message is recorded against the person it
        // went to, so a guard that looked for an empty scope found nothing at
        // all and waved through exactly the repeats it exists to catch.
        let Some((_, at, evidence, scope)) =
            errand_core::db::recent_commit_of_any_scope(state.pool(), &task.id, kind, minutes)
                .await
                .map_err(ApiError::from)?
        else {
            continue;
        };
        // Name the person when the scope is one, because "already messaged Mum"
        // is a fact somebody can check and "already sent a message" is not.
        let what = match named_scope(state, &task.id, &scope).await {
            Some(label) => format!("{kind} to {label}"),
            None => (*kind).to_string(),
        };
        return Ok(Some(format!(
            "This task already carried out a {what} at {at}: {}. The new schedule's first run, at \
             {}, comes round sooner than the old one would have, and Errand knows what it has \
             already done by the exact time the run was due — so that run would look like fresh \
             work and could do the {kind} a second time. Nothing has been changed. Check the site \
             first if that would matter, then confirm if you still want the change.",
            evidence.unwrap_or_else(|| "no details were recorded".into()),
            new_first.to_rfc3339()
        )));
    }
    Ok(None)
}

/// The person a recorded action was aimed at, if the scope names one.
///
/// A scope is only ever a recipient id or nothing, and a stranger's id in a
/// sentence helps nobody, so anything that does not resolve to somebody this
/// task may write to comes back as `None` and is left out.
async fn named_scope(state: &AppState, task_id: &str, scope: &str) -> Option<String> {
    if scope.is_empty() {
        return None;
    }
    errand_core::db::recipients_for_task(state.pool(), task_id)
        .await
        .ok()?
        .into_iter()
        .find(|r| r.id == scope)
        .map(|r| r.label)
}

/// Say what the engine will really do with a schedule, before it is saved.
///
/// A form that builds a cron expression can be wrong in ways nobody notices
/// until the morning nothing happens. This is the other half of the loop: the
/// form says what was meant, this says what the engine will do, and the two can
/// disagree in front of the person instead of at 08:00 next Tuesday.
///
/// It never fails. A schedule Errand cannot use comes back as `valid: false`
/// with the reason in plain words, because a 500 here would tell the form
/// nothing at all.
async fn preview_schedule(
    Extension(caller): Extension<Caller>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Read)?;

    let spec = match errand_core::schedule::ScheduleSpec::from_json(&body) {
        Ok(s) => s,
        Err(e) => {
            return Ok(Json(json!({
                "valid": false,
                "describes": "",
                "preview": [],
                "problem": format!(
                    "Errand cannot read that as a schedule, so it has not been saved: {e}"
                ),
            })))
        }
    };
    if let Err(e) = spec.validate() {
        return Ok(Json(json!({
            "valid": false,
            "describes": spec.describe(),
            "preview": [],
            "problem": e.to_string(),
        })));
    }

    let (preview, mut problem) = match spec.preview(chrono::Utc::now(), PREVIEW_COUNT) {
        Ok(list) => (
            list.iter().map(|d| d.to_rfc3339()).collect::<Vec<_>>(),
            None,
        ),
        Err(e) => (vec![], Some(e.to_string())),
    };
    // A schedule that is perfectly legal and has no runs left is the one a
    // person most needs telling about, because the form looks right and nothing
    // will ever happen.
    if problem.is_none() && preview.is_empty() && spec.is_scheduled() {
        problem = Some(
            "Errand can read this schedule, but it has no runs left to come: a one-off whose \
             time has already passed never happens again. Choose a time in the future."
                .to_string(),
        );
    }

    Ok(Json(json!({
        "valid": problem.is_none(),
        "describes": spec.describe(),
        "preview": preview,
        "problem": problem,
    })))
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
    // The moment the run would actually begin, not the bare occurrence. The
    // sweep stores the started instant, so storing the occurrence here made the
    // countdown jump the first time the scheduler ticked — by up to the whole
    // jitter, on a screen whose entire job is to say when this will happen.
    let next = spec
        .next_after(chrono::Utc::now())
        .map_err(|e| ApiError::bad_request(e.to_string()))?
        .map(|occurrence| crate::scheduler::start_instant(&task, &spec, occurrence));

    if spec.is_scheduled()
        && errand_core::db::active_playbook(state.pool(), &id)
            .await
            .map_err(ApiError::from)?
            .is_none()
    {
        return Err(ApiError::conflict("task_not_taught", NOT_TAUGHT));
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

    // Written either way. A one-off whose moment has gone has no next run, and
    // saying so clears the time it ran at; skipping the write would put the old
    // value back after the sweep had just cleared it, and the task page would go
    // on counting down to a moment in the past.
    let _ = errand_core::db::set_next_run_at(
        state.pool(),
        &id,
        next.map(|n| n.to_rfc3339()).as_deref(),
    )
    .await;
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

    // Every setting is checked and tidied before any of them is written, so a
    // request with one good value and one bad one changes nothing rather than
    // half of what was asked for.
    let mut to_write = vec![];
    let mut notes = vec![];
    for (k, v) in &body.settings {
        let (value, note) = check_setting(k, v).map_err(ApiError::bad_request)?;
        notes.extend(note);
        to_write.push((k.clone(), value));
    }
    let mut settings_written = vec![];
    for (k, v) in to_write {
        errand_core::db::set_setting(state.pool(), &k, &v)
            .await
            .map_err(ApiError::from)?;
        settings_written.push(k);
    }

    let health = crate::channels::health_all(state.pool()).await;
    Ok(Json(json!({
        "stored": stored,
        "settings": settings_written,
        "notes": notes,
        "health": health.iter().find(|h| h.channel == channel),
        "note": "Secrets are in your macOS keychain. Errand can use them; it cannot show them \
                 back to you."
    })))
}

/// The non-secret settings this route will write, and nothing else.
///
/// It used to take any key at all, which meant a key spelled slightly wrong was
/// stored happily, read by nothing, and the person believed they had changed
/// something they had not.
const ALLOWED_SETTINGS: &[&str] = &["messaging.quiet", "messaging.whatsapp.base_url"];

/// The word the outbox actually looks for when deciding whether bad news wakes
/// you up. See `quiet_hours` in runner/src/outbox.rs: anything else is stored,
/// read by nobody, and quietly ignored.
const QUIET_BREAKS_THROUGH: &str = "failure_breaks_through";

/// The same idea spelled the way the settings screen sends and reads it.
///
/// The two names disagree today. Rather than pick one and silently drop what
/// the other side says, this route accepts either on the way in and stores the
/// one the outbox reads, and `GET /v1/settings` shows both on the way out. The
/// alternative is a switch that a person turns off and that turns itself back
/// on when they next open the page.
const QUIET_BREAKS_THROUGH_ALT: &str = "failures_break_through";

/// Check one setting and hand back the tidied value, plus anything worth saying
/// about what it will actually do.
fn check_setting(
    key: &str,
    value: &serde_json::Value,
) -> Result<(serde_json::Value, Option<String>), String> {
    if key == "messaging.quiet" {
        check_quiet_hours(value)
    } else if key == crate::channels::whatsapp::SETTING_BASE_URL {
        Ok((check_gateway_url(value)?, None))
    } else {
        Err(format!(
            "'{key}' is not a setting Errand keeps, so nothing has been saved. The ones it does \
             keep are: {}.",
            ALLOWED_SETTINGS.join(", ")
        ))
    }
}

/// Quiet hours, in the exact shape the outbox reads them in.
fn check_quiet_hours(v: &serde_json::Value) -> Result<(serde_json::Value, Option<String>), String> {
    let Some(obj) = v.as_object() else {
        return Err(
            "Quiet hours need a from hour, a to hour, and whether failures should still reach \
             you during them. Nothing has been saved."
                .into(),
        );
    };
    for k in obj.keys() {
        if !["from", "to", QUIET_BREAKS_THROUGH, QUIET_BREAKS_THROUGH_ALT].contains(&k.as_str()) {
            return Err(format!(
                "'{k}' is not part of quiet hours, so it would have been stored and then read by \
                 nothing at all. The parts are: from, to, and {QUIET_BREAKS_THROUGH}."
            ));
        }
    }

    let hour = |name: &str| -> Result<u64, String> {
        let raw = obj.get(name).ok_or_else(|| {
            format!(
                "Quiet hours need a '{name}' hour, so nothing has been saved. Give both, as \
                 whole hours of the day from 0 to 23 — 22 and 7 for an ordinary night."
            )
        })?;
        let n = raw.as_u64().ok_or_else(|| {
            format!(
                "The '{name}' hour is {raw}, which is not a whole hour of the day. Hours run \
                 from 0 to 23, so 22 means ten at night."
            )
        })?;
        if n > 23 {
            return Err(format!(
                "The '{name}' hour is {n}, and hours run from 0 to 23, so 22 means ten at night. \
                 Nothing has been saved."
            ));
        }
        Ok(n)
    };
    let from = hour("from")?;
    let to = hour("to")?;

    let breaks = match (
        obj.get(QUIET_BREAKS_THROUGH),
        obj.get(QUIET_BREAKS_THROUGH_ALT),
    ) {
        (None, None) => true,
        (Some(a), Some(b)) if a != b => {
            return Err(format!(
                "Quiet hours were given '{QUIET_BREAKS_THROUGH}' as {a} and \
                 '{QUIET_BREAKS_THROUGH_ALT}' as {b}. Those are two names for the same switch and \
                 they do not agree, so nothing has been saved rather than Errand choosing for \
                 you. Send just one of them."
            ))
        }
        (a, b) => match a.or(b) {
            Some(serde_json::Value::Bool(x)) => *x,
            Some(other) => {
                return Err(format!(
                    "Whether failures break through quiet hours is {other}, which is neither true \
                     nor false. Set it to true to be told about a failure even at night, or false \
                     to hear about it in the morning."
                ))
            }
            None => true,
        },
    };

    // Equal hours are legal and mean no quiet period at all, which is how you
    // switch this off. Said out loud, because saving "22 to 22" and getting
    // messages all night would otherwise look like a fault.
    let note = (from == to).then(|| {
        "The quiet period starts and ends at the same hour, which means there is no quiet period: \
         everything goes out as soon as it is ready."
            .to_string()
    });

    Ok((
        json!({ "from": from, "to": to, QUIET_BREAKS_THROUGH: breaks }),
        note,
    ))
}

/// Where the WhatsApp gateway lives.
///
/// The trailing slash is taken off here because the sender builds its addresses
/// as `{base}/sessions`, and a doubled slash is the sort of thing that fails
/// only at the moment a message was supposed to go out.
fn check_gateway_url(v: &serde_json::Value) -> Result<serde_json::Value, String> {
    let Some(raw) = v.as_str() else {
        return Err(
            "The WhatsApp gateway address has to be the address itself, as text. Nothing has \
             been saved."
                .into(),
        );
    };
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(
            "The WhatsApp gateway address is empty, so Errand would have nowhere to send. Type \
             the address the gateway prints when it starts, or leave WhatsApp switched off and \
             use Telegram."
                .into(),
        );
    }
    let parsed = url::Url::parse(trimmed).map_err(|_| {
        format!(
            "'{trimmed}' is not an address Errand can call, so nothing has been saved. It needs \
             the whole thing, starting with http:// or https://."
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(format!(
            "'{trimmed}' does not look like a web address Errand can call, so nothing has been \
             saved. It needs to start with http:// or https:// and name the machine the gateway \
             is running on."
        ));
    }
    Ok(json!(trimmed))
}

/// Every setting a person can change, and what it is set to now.
///
/// Only the writable ones. Errand's own bookkeeping lives in the same table,
/// and a screen that showed it would be inviting somebody to change a number
/// that means nothing to them.
async fn get_settings(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Read)?;
    let mut out = serde_json::Map::new();
    for key in ALLOWED_SETTINGS {
        let stored = errand_core::db::get_setting(state.pool(), key)
            .await
            .map_err(ApiError::from)?;
        let Some(mut value) = stored.or_else(|| {
            // Quiet hours are in force from the moment of install, whether or
            // not anyone has saved them. A screen that showed nothing here
            // would have a person believe messages can go out at three in the
            // morning, when the outbox is in fact already holding them.
            (*key == "messaging.quiet").then(crate::outbox::default_quiet_hours)
        }) else {
            continue;
        };
        // Shown under both names, for the reason given at QUIET_BREAKS_THROUGH_ALT.
        if *key == "messaging.quiet" {
            if let Some(b) = value.get(QUIET_BREAKS_THROUGH).cloned() {
                if let Some(obj) = value.as_object_mut() {
                    obj.insert(QUIET_BREAKS_THROUGH_ALT.to_string(), b);
                }
            }
        }
        out.insert((*key).to_string(), value);
    }
    Ok(Json(serde_json::Value::Object(out)))
}

// -------------------------------------------------------------- recipients --
//
// Two separate decisions, deliberately. Saving somebody's address is
// housekeeping. Letting one task write to them is a grant, and it is the thing
// that decides whether a real message reaches a real person, so it needs the
// same permission as approving a booking rather than the one that edits
// settings.

async fn list_recipients(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Manage)?;
    let items = errand_core::db::list_recipients(state.pool())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({
        "items": items,
        "note": "This is your own address book, so the addresses are shown in full. A task's \
                 agent is only ever shown the masked form, because an address it never sees is \
                 an address it cannot give away."
    })))
}

#[derive(Deserialize)]
struct NewRecipient {
    label: String,
    channel: String,
    address: String,
}

async fn create_recipient(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Json(body): Json<NewRecipient>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Manage)?;
    if crate::channels::ChannelId::parse(&body.channel).is_none() {
        return Err(ApiError::bad_request(format!(
            "'{}' is not a way Errand can send messages, so this contact was not saved. Choose \
             telegram, whatsapp, apple_mail or imessage.",
            body.channel
        )));
    }
    if body.label.trim().is_empty() {
        return Err(ApiError::bad_request(
            "Give this contact a name you will recognise later, such as 'Mum'. It is the name a \
             task's agent sees when it picks who to write to.",
        ));
    }
    check_address(&body.channel, &body.address).map_err(ApiError::bad_request)?;

    let id = errand_core::db::create_recipient(
        state.pool(),
        &body.label,
        &body.channel,
        body.address.trim(),
    )
    .await
    .map_err(ApiError::from)?;

    Ok(Json(json!({
        "id": id,
        "label": body.label.trim(),
        "channel": body.channel,
        "address": body.address.trim(),
        "address_masked": errand_core::db::masked_address(&body.channel, &body.address),
        "note": "Saved. No task can write to them yet: that is a separate step, done on the task \
                 itself."
    })))
}

async fn delete_recipient(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Manage)?;
    let gone = errand_core::db::delete_recipient(state.pool(), &id)
        .await
        .map_err(ApiError::from)?;
    if !gone {
        return Err(ApiError::not_found("No contact with that id."));
    }
    Ok(Json(json!({
        "deleted": id,
        "note": "Forgotten, and every task that could write to them no longer can. Messages that \
                 have already been sent are not affected."
    })))
}

async fn list_task_recipients(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Manage)?;
    if errand_core::db::get_task(state.pool(), &id)
        .await
        .map_err(ApiError::from)?
        .is_none()
    {
        return Err(ApiError::not_found(format!("No task with id {id}.")));
    }
    let items = errand_core::db::recipients_for_task(state.pool(), &id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({
        "items": items,
        "note": "These are the only people this task may write to. It cannot be given an address \
                 by a web page, or by anything it reads."
    })))
}

#[derive(Deserialize)]
struct GrantRecipient {
    recipient_id: String,
    /// Both on by default: somebody added to a task is presumed to want to hear
    /// how it went either way.
    #[serde(default = "yes")]
    on_success: bool,
    #[serde(default = "yes")]
    on_failure: bool,
}

async fn grant_recipient(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Json(body): Json<GrantRecipient>,
) -> ApiResult<Json<serde_json::Value>> {
    // Approve rather than manage. Linking a person to a task decides whether a
    // real message goes to a real third party without anybody watching, which
    // is the same class of decision as resolving a hold, and a client that can
    // change settings should not be able to make it.
    require(&caller, Scope::Approve)?;

    if errand_core::db::get_task(state.pool(), &id)
        .await
        .map_err(ApiError::from)?
        .is_none()
    {
        return Err(ApiError::not_found(format!("No task with id {id}.")));
    }
    let person = errand_core::db::list_recipients(state.pool())
        .await
        .map_err(ApiError::from)?
        .into_iter()
        .find(|r| r.id == body.recipient_id)
        .ok_or_else(|| {
            ApiError::not_found(
                "No contact with that id, so nothing was granted. Add them to your address book \
                 first.",
            )
        })?;

    if !body.on_success && !body.on_failure {
        return Err(ApiError::bad_request(
            "This would let the task contact them about nothing at all. Choose whether they hear \
             when it works, when it fails, or both — or remove them from the task instead.",
        ));
    }

    errand_core::db::link_recipient(
        state.pool(),
        &id,
        &body.recipient_id,
        body.on_success,
        body.on_failure,
    )
    .await
    .map_err(ApiError::from)?;

    state.emit(Event::TaskUpdated {
        task_id: id.clone(),
    });
    Ok(Json(json!({
        "task_id": id,
        "recipient_id": body.recipient_id,
        "on_success": body.on_success,
        "on_failure": body.on_failure,
        "note": format!(
            "This task may now write to {} at {}, and to nobody else you have not added. It is \
             never told the address itself.",
            person.label, person.address_masked
        )
    })))
}

async fn revoke_recipient(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path((id, recipient_id)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    // Manage is enough to take a permission away. Removing one can only ever
    // mean fewer messages reach fewer people.
    require(&caller, Scope::Manage)?;
    let gone = errand_core::db::unlink_recipient(state.pool(), &id, &recipient_id)
        .await
        .map_err(ApiError::from)?;
    if !gone {
        return Err(ApiError::not_found(
            "This task was not able to write to that contact anyway, so nothing changed.",
        ));
    }
    state.emit(Event::TaskUpdated {
        task_id: id.clone(),
    });
    Ok(Json(json!({
        "task_id": id,
        "recipient_id": recipient_id,
        "note": "This task can no longer write to them. The contact itself is untouched, and \
                 every other task that has them keeps them."
    })))
}

/// Could this channel actually deliver to this address?
///
/// Checked when it is typed rather than when a message is due. A typo found now
/// costs ten seconds; the same typo found at send time costs the message, and
/// the person only finds out because the reply never came.
fn check_address(channel: &str, address: &str) -> Result<(), String> {
    let a = address.trim();
    if a.is_empty() {
        return Err(
            "This contact has no address, so nothing could ever be sent to them. Add the phone \
             number, email address or chat id for the way you picked."
                .into(),
        );
    }
    match channel {
        "apple_mail" => looks_like_email(a).then_some(()).ok_or_else(|| {
            format!(
                "'{a}' does not look like an email address, so Mail would have nowhere to send \
                 it. Type it the way it appears in your address book, like \
                 someone@example.com."
            )
        }),
        "whatsapp" | "imessage" => (looks_like_email(a) || looks_like_phone(a))
            .then_some(())
            .ok_or_else(|| {
                format!(
                    "'{a}' is neither a phone number nor an email address, and {} reaches people \
                     by one or the other. Type the number with its country code, like \
                     +1 555 0100.",
                    if channel == "whatsapp" {
                        "WhatsApp"
                    } else {
                        "Messages"
                    }
                )
            }),
        "telegram" => looks_like_telegram_chat(a).then_some(()).ok_or_else(|| {
            format!(
                "'{a}' is not a Telegram chat id. A chat id is a number, sometimes starting with \
                 a minus sign for a group, such as 123456789; a channel can also be written as \
                 @itsname. A phone number will not work, because Telegram does not let a bot \
                 start a conversation from one — message the bot once and it will tell you the id."
            )
        }),
        other => Err(format!(
            "'{other}' is not a way Errand can send messages. Choose telegram, whatsapp, \
             apple_mail or imessage."
        )),
    }
}

fn looks_like_email(a: &str) -> bool {
    match a.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && !a.chars().any(char::is_whitespace)
        }
        None => false,
    }
}

/// A phone number written the way people write them, punctuation and all.
fn looks_like_phone(a: &str) -> bool {
    let digits = a.chars().filter(char::is_ascii_digit).count();
    (7..=15).contains(&digits)
        && a.chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '+' | ' ' | '-' | '(' | ')' | '.'))
}

/// A Telegram chat id: a number, negative for a group, or a channel's @name.
fn looks_like_telegram_chat(a: &str) -> bool {
    if let Some(handle) = a.strip_prefix('@') {
        return handle.len() >= 5
            && handle
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_');
    }
    let digits = a.strip_prefix('-').unwrap_or(a);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
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
    let d = crate::models::discover(q.scan_network).await;
    Ok(Json(json!({
        "found": d.found,
        // Said back, so nobody has to infer from an empty list whether the scan
        // they asked for actually happened, or how hard it looked.
        "looked_at": scanned_where(q.scan_network),
        "addresses": d.addresses,
        "ports": d.ports,
        // When macOS is refusing the local network, an empty result says
        // nothing about what is out there, and must not be read as if it did.
        "blocked": d.blocked,
        // Answered, but not usable as it stands. Reported rather than dropped:
        // a server that only needs a key looks exactly like an empty network if
        // nobody says otherwise.
        "also_seen": d.also_seen
            .iter()
            .map(|(url, why)| json!({ "url": url, "why": why }))
            .collect::<Vec<_>>(),
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

#[cfg(test)]
mod tests {
    use super::super::testkit::{self, a_ready_manual_task, a_task};
    use serde_json::json;

    /// A cron that comes round every minute, so a test never has to wait for a
    /// particular hour and never depends on what time it is run.
    const EVERY_MINUTE: &str = "0 * * * * *";

    // ------------------------------------------------------- sites a task may open --

    #[tokio::test]
    async fn a_site_list_is_stored_the_way_the_browser_will_compare_it() {
        let api = testkit::start().await;
        let id = a_task(
            &api,
            json!({
                "name": "Courts",
                "description": "Book a court.",
                "allowed_domains": ["HTTPS://Example.COM/basket/", "  tennis-club.example  "]
            }),
        )
        .await;

        let task = api.get(&format!("/v1/tasks/{id}")).await;
        assert_eq!(
            task["allowed_domains"],
            json!(["example.com", "tennis-club.example"]),
            "a pasted URL must be reduced to the bare host the check compares"
        );

        let (code, body) = api
            .patch(
                &format!("/v1/tasks/{id}"),
                json!({ "allowed_domains": ["Example.Com.", "https://other.example:8443/x"] }),
            )
            .await;
        assert_eq!(code, 200, "{body}");
        assert_eq!(
            api.get(&format!("/v1/tasks/{id}")).await["allowed_domains"],
            json!(["example.com", "other.example"]),
            "an edited list must be tidied the same way a new one is"
        );
    }

    #[tokio::test]
    async fn a_wildcard_site_is_refused_with_something_to_type_instead() {
        let api = testkit::start().await;
        let id = a_task(
            &api,
            json!({
                "name": "Courts",
                "description": "Book a court.",
                "allowed_domains": ["example.com"]
            }),
        )
        .await;

        let (code, body) = api
            .patch(
                &format!("/v1/tasks/{id}"),
                json!({ "allowed_domains": ["*.example.org"] }),
            )
            .await;
        assert_eq!(code, 400, "{body}");
        let detail = body["detail"].as_str().unwrap_or_default();
        assert!(
            detail.contains("*.example.org"),
            "the refusal must name the entry that was wrong: {detail}"
        );
        assert!(
            detail.contains("Type example.org instead"),
            "the refusal must say what to type instead: {detail}"
        );
        assert_eq!(
            api.get(&format!("/v1/tasks/{id}")).await["allowed_domains"],
            json!(["example.com"]),
            "one bad entry must leave the saved list exactly as it was"
        );
    }

    #[tokio::test]
    async fn allowing_only_the_www_form_is_saved_but_said_out_loud() {
        let api = testkit::start().await;
        let id = a_task(&api, json!({ "name": "N", "description": "d" })).await;

        let (code, body) = api
            .patch(
                &format!("/v1/tasks/{id}"),
                json!({ "allowed_domains": ["www.example.com"] }),
            )
            .await;
        assert_eq!(code, 200, "{body}");
        assert_eq!(body["task"]["allowed_domains"], json!(["www.example.com"]));
        let warnings = body["warnings"].as_array().expect("a warnings list");
        assert!(
            warnings.iter().any(|w| w
                .as_str()
                .unwrap_or_default()
                .contains("Adding example.com")),
            "matching runs one way only, so this has to be said: {warnings:?}"
        );
    }

    // -------------------------------------------- the gate in front of a schedule --

    #[tokio::test]
    async fn an_untaught_task_cannot_be_moved_onto_a_schedule() {
        // The hole this closes: activating checks for a playbook only when the
        // task is already scheduled, so a manual task activates happily and
        // could then be edited onto a cron with nothing in the way.
        let api = testkit::start().await;
        let id = a_task(
            &api,
            json!({ "name": "Untaught", "description": "never taught" }),
        )
        .await;
        let (code, body) = api
            .post(&format!("/v1/tasks/{id}/activate"), json!({}))
            .await;
        assert_eq!(
            code, 200,
            "a manual task activates without a playbook: {body}"
        );

        let (code, body) = api
            .patch(
                &format!("/v1/tasks/{id}"),
                json!({ "schedule": { "kind": "cron", "expr": EVERY_MINUTE, "tz": "UTC" } }),
            )
            .await;
        assert_eq!(code, 409, "{body}");
        assert_eq!(body["code"], "task_not_taught");
        assert!(
            body["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("Teach it once"),
            "the refusal must say what to do about it: {body}"
        );

        let task = api.get(&format!("/v1/tasks/{id}")).await;
        assert_eq!(
            task["schedule"]["kind"], "manual",
            "a refused change must leave the schedule alone"
        );
    }

    #[tokio::test]
    async fn a_taught_task_can_be_put_on_a_schedule() {
        let api = testkit::start().await;
        let id = a_ready_manual_task(&api).await;
        let (code, body) = api
            .patch(
                &format!("/v1/tasks/{id}"),
                json!({ "schedule": { "kind": "cron", "expr": EVERY_MINUTE, "tz": "UTC" } }),
            )
            .await;
        assert_eq!(code, 200, "{body}");
        assert_eq!(body["task"]["schedule"]["kind"], "cron");
    }

    // ------------------------------------- doing something twice by moving the clock --

    #[tokio::test]
    async fn moving_a_schedule_over_work_already_done_stops_and_says_why() {
        let api = testkit::start().await;
        let id = a_ready_manual_task(&api).await;

        // A run that actually booked something, recorded the way the agent's
        // own tools record it.
        let run = errand_core::db::try_create_run(
            &api.pool,
            &id,
            &format!("manual/{}", errand_core::new_id()),
            "manual",
            "normal",
            None,
        )
        .await
        .expect("a run");
        let armed = errand_core::db::arm_side_effect(
            &api.pool,
            &run.id,
            &id,
            &run.occurrence_id,
            "booking",
            "",
        )
        .await
        .expect("arming the guard");
        let errand_core::db::FenceVerdict::Armed(fence) = armed else {
            panic!("a fresh slot must arm");
        };
        errand_core::db::commit_side_effect(&api.pool, &fence, "court 2 at 19:00")
            .await
            .expect("recording that it happened");

        let change = json!({ "schedule": { "kind": "cron", "expr": EVERY_MINUTE, "tz": "UTC" } });
        let (code, body) = api.patch(&format!("/v1/tasks/{id}"), change.clone()).await;
        assert_eq!(code, 409, "{body}");
        assert_eq!(body["code"], "schedule_change_may_repeat");
        let detail = body["detail"].as_str().unwrap_or_default();
        assert!(
            detail.contains("court 2 at 19:00"),
            "it must say what was already done: {detail}"
        );
        assert_eq!(
            api.get(&format!("/v1/tasks/{id}")).await["schedule"]["kind"],
            "manual",
            "a refusal must change nothing"
        );

        // And once it has been read, the person can say yes anyway.
        let mut acknowledged = change;
        acknowledged["acknowledge_repeat"] = json!(true);
        let (code, body) = api.patch(&format!("/v1/tasks/{id}"), acknowledged).await;
        assert_eq!(code, 200, "{body}");
        assert_eq!(body["task"]["schedule"]["kind"], "cron");
    }

    #[tokio::test]
    async fn moving_a_schedule_over_a_message_already_sent_stops_and_names_the_person() {
        // The failure this guards against: the guard asked whether this task had
        // recently done something with no recipient attached, and every message
        // is recorded against the person it went to. So it looked at the one
        // column that could never match and let the change through, and Mum was
        // written to twice about the same thing.
        let api = testkit::start().await;
        let id = a_ready_manual_task(&api).await;

        let (code, person) = api
            .post(
                "/v1/recipients",
                json!({ "label": "Mum", "channel": "apple_mail", "address": "mum@example.com" }),
            )
            .await;
        assert_eq!(code, 200, "saving the person failed: {person}");
        let rid = person["id"].as_str().expect("a recipient id").to_string();
        let (code, granted) = api
            .post(
                &format!("/v1/tasks/{id}/recipients"),
                json!({ "recipient_id": rid, "on_success": true, "on_failure": true }),
            )
            .await;
        assert_eq!(code, 200, "granting the task permission failed: {granted}");

        // A run that really did write to her, recorded exactly the way the
        // outbox and the agent's own tool record it: against her id.
        let run = errand_core::db::try_create_run(
            &api.pool,
            &id,
            &format!("manual/{}", errand_core::new_id()),
            "manual",
            "normal",
            None,
        )
        .await
        .expect("a run");
        let armed = errand_core::db::arm_side_effect(
            &api.pool,
            &run.id,
            &id,
            &run.occurrence_id,
            "message",
            &rid,
        )
        .await
        .expect("arming the guard");
        let errand_core::db::FenceVerdict::Armed(fence) = armed else {
            panic!("a fresh slot must arm");
        };
        errand_core::db::commit_side_effect(&api.pool, &fence, "told her the court was booked")
            .await
            .expect("recording that it happened");

        let (code, body) = api
            .patch(
                &format!("/v1/tasks/{id}"),
                json!({ "schedule": { "kind": "cron", "expr": EVERY_MINUTE, "tz": "UTC" } }),
            )
            .await;
        assert_eq!(code, 409, "{body}");
        assert_eq!(body["code"], "schedule_change_may_repeat");
        let detail = body["detail"].as_str().unwrap_or_default();
        assert!(
            detail.contains("Mum"),
            "it must name the person who would hear it all over again: {detail}"
        );
        assert!(
            detail.contains("told her the court was booked"),
            "it must say what was already done: {detail}"
        );
        assert_eq!(
            api.get(&format!("/v1/tasks/{id}")).await["schedule"]["kind"],
            "manual",
            "a refusal must change nothing"
        );
    }

    // ------------------------------------------------- saying a schedule in words --

    #[tokio::test]
    async fn a_task_says_its_schedule_in_words_and_when_it_will_next_run() {
        let api = testkit::start().await;
        let id = a_task(
            &api,
            json!({
                "name": "Morning",
                "description": "d",
                "schedule": { "kind": "cron", "expr": "0 0 8 * * *", "tz": "UTC" }
            }),
        )
        .await;

        let task = api.get(&format!("/v1/tasks/{id}")).await;
        assert!(
            task["schedule_describes"]
                .as_str()
                .unwrap_or_default()
                .contains("Every day at 08:00"),
            "no screen should have to read a cron expression: {task}"
        );
        assert_eq!(
            task["schedule_preview"].as_array().map(Vec::len),
            Some(3),
            "the next few runs must be shown, not computed in the interface"
        );

        let listed = api.get("/v1/tasks").await;
        assert!(
            listed["items"][0]["schedule_describes"].is_string(),
            "the list says it too, or the list has to work it out itself"
        );
    }

    #[tokio::test]
    async fn a_schedule_the_engine_cannot_use_is_reported_rather_than_crashed_on() {
        let api = testkit::start().await;
        let (code, body) = api
            .post(
                "/v1/schedule/preview",
                json!({ "kind": "cron", "expr": "quarter past banana", "tz": "UTC" }),
            )
            .await;
        assert_eq!(
            code, 200,
            "a bad schedule is an answer, not a fault: {body}"
        );
        assert_eq!(body["valid"], false);
        assert!(
            !body["problem"].as_str().unwrap_or_default().is_empty(),
            "an invalid schedule must come back with the reason: {body}"
        );
    }

    #[tokio::test]
    async fn the_preview_says_what_the_engine_will_do_not_what_the_form_meant() {
        let api = testkit::start().await;
        let (code, body) = api
            .post(
                "/v1/schedule/preview",
                json!({ "kind": "cron", "expr": "0 30 6 * * WED", "tz": "UTC" }),
            )
            .await;
        assert_eq!(code, 200, "{body}");
        assert_eq!(body["valid"], true);
        assert!(body["describes"]
            .as_str()
            .unwrap_or_default()
            .starts_with("Every Wednesday at 06:30"));
        assert_eq!(body["preview"].as_array().map(Vec::len), Some(3));
    }

    #[tokio::test]
    async fn a_one_off_whose_time_has_gone_is_not_offered_as_a_working_schedule() {
        // Valid, readable, and it will never happen. That is the shape a form
        // gets right and a person finds out about a week later.
        let api = testkit::start().await;
        let (_, body) = api
            .post(
                "/v1/schedule/preview",
                json!({ "kind": "once", "at": "2020-01-01T08:00:00", "tz": "UTC" }),
            )
            .await;
        assert_eq!(body["valid"], false);
        assert!(body["problem"]
            .as_str()
            .unwrap_or_default()
            .contains("no runs left to come"));
    }

    // ----------------------------------------------------- what an edit may not do --

    #[tokio::test]
    async fn an_edit_cannot_blank_the_name_that_creating_one_insisted_on() {
        let api = testkit::start().await;
        let id = a_task(&api, json!({ "name": "Courts", "description": "d" })).await;
        let (code, body) = api
            .patch(&format!("/v1/tasks/{id}"), json!({ "name": "   " }))
            .await;
        assert_eq!(code, 400, "{body}");
        assert_eq!(api.get(&format!("/v1/tasks/{id}")).await["name"], "Courts");
    }

    #[tokio::test]
    async fn an_archived_task_refuses_an_edit_instead_of_half_applying_it() {
        let api = testkit::start().await;
        let id = a_task(&api, json!({ "name": "Old", "description": "d" })).await;
        // Archiving has no route of its own yet; this is the call the rest of
        // the daemon uses to move a task's status.
        errand_core::db::set_task_status(&api.pool, &id, "archived")
            .await
            .expect("archiving");

        let (code, body) = api
            .patch(&format!("/v1/tasks/{id}"), json!({ "name": "New" }))
            .await;
        assert_eq!(code, 409, "{body}");
        assert_eq!(body["code"], "task_archived");
        assert_eq!(api.get(&format!("/v1/tasks/{id}")).await["name"], "Old");
    }

    #[tokio::test]
    async fn a_retried_save_is_the_same_save_rather_than_a_second_one() {
        let api = testkit::start().await;
        let id = a_ready_manual_task(&api).await;
        let change = json!({ "schedule": { "kind": "cron", "expr": EVERY_MINUTE, "tz": "UTC" } });

        let (first_code, first) = api
            .patch_with_key(&format!("/v1/tasks/{id}"), change.clone(), "save-1")
            .await;
        assert_eq!(first_code, 200, "{first}");
        let (again_code, again) = api
            .patch_with_key(&format!("/v1/tasks/{id}"), change, "save-1")
            .await;
        assert_eq!(again_code, 200, "{again}");
        assert_eq!(
            first, again,
            "a retry after a dropped connection must give back the same answer"
        );

        // The same key for something else is a client bug, and replaying the
        // old answer would hide it.
        let (code, body) = api
            .patch_with_key(
                &format!("/v1/tasks/{id}"),
                json!({ "name": "Other" }),
                "save-1",
            )
            .await;
        assert_eq!(code, 409, "{body}");
        assert_eq!(body["code"], "idempotency_key_reuse");
    }

    // -------------------------------------------------- people a task may message --

    #[tokio::test]
    async fn an_address_that_could_never_be_delivered_to_is_refused_when_it_is_typed() {
        let api = testkit::start().await;
        for (channel, address) in [
            ("apple_mail", "not-an-email"),
            ("whatsapp", "ring me at the club"),
            ("telegram", "+15550100"),
        ] {
            let (code, body) = api
                .post(
                    "/v1/recipients",
                    json!({ "label": "Someone", "channel": channel, "address": address }),
                )
                .await;
            assert_eq!(code, 400, "{channel} accepted '{address}': {body}");
            assert!(
                body["detail"]
                    .as_str()
                    .unwrap_or_default()
                    .contains(address),
                "the refusal must quote what was typed: {body}"
            );
        }

        let (code, body) = api
            .post(
                "/v1/recipients",
                json!({ "label": "Mum", "channel": "apple_mail", "address": "mum@example.com" }),
            )
            .await;
        assert_eq!(code, 200, "{body}");
        assert_eq!(
            body["address_masked"], "m•••@example.com",
            "the agent is shown enough to recognise, never enough to write to"
        );
    }

    #[tokio::test]
    async fn letting_a_task_message_somebody_needs_the_permission_that_approves_things() {
        let api = testkit::start().await;
        let id = a_ready_manual_task(&api).await;
        let (_, person) = api
            .post(
                "/v1/recipients",
                json!({ "label": "Mum", "channel": "apple_mail", "address": "mum@example.com" }),
            )
            .await;
        let person_id = person["id"].as_str().expect("a contact id").to_string();

        // A token that can change settings but not approve anything.
        let settings_only = testkit::mint(&api.pool, "settings-only", "read,manage").await;
        let (code, body) = api
            .as_token(
                &settings_only,
                reqwest::Method::POST,
                &format!("/v1/tasks/{id}/recipients"),
                Some(json!({ "recipient_id": person_id })),
                None,
            )
            .await;
        assert_eq!(
            code, 403,
            "deciding that a real message reaches a real person is an approval: {body}"
        );

        let (code, body) = api
            .post(
                &format!("/v1/tasks/{id}/recipients"),
                json!({ "recipient_id": person_id, "on_success": true, "on_failure": false }),
            )
            .await;
        assert_eq!(code, 200, "{body}");

        // Taking it away again is always safe, so managing is enough.
        let (code, body) = api
            .as_token(
                &settings_only,
                reqwest::Method::DELETE,
                &format!("/v1/tasks/{id}/recipients/{person_id}"),
                None,
                None,
            )
            .await;
        assert_eq!(
            code, 200,
            "removing a permission must never be blocked: {body}"
        );
    }

    #[tokio::test]
    async fn a_grant_belongs_to_one_task_and_no_other_task_inherits_it() {
        let api = testkit::start().await;
        let allowed = a_ready_manual_task(&api).await;
        let other = a_task(&api, json!({ "name": "Other", "description": "d" })).await;
        let (_, person) = api
            .post(
                "/v1/recipients",
                json!({ "label": "Mum", "channel": "apple_mail", "address": "mum@example.com" }),
            )
            .await;
        let person_id = person["id"].as_str().expect("a contact id").to_string();

        api.post(
            &format!("/v1/tasks/{allowed}/recipients"),
            json!({ "recipient_id": person_id }),
        )
        .await;

        let theirs = api.get(&format!("/v1/tasks/{allowed}/recipients")).await;
        assert_eq!(theirs["items"][0]["id"], json!(person_id));
        let others = api.get(&format!("/v1/tasks/{other}/recipients")).await;
        assert!(
            others["items"].as_array().expect("a list").is_empty(),
            "a permission given to one task must not appear on another: {others}"
        );

        // Forgetting the contact takes every task's permission with them.
        assert_eq!(
            api.delete(&format!("/v1/recipients/{person_id}")).await.0,
            200
        );
        let theirs = api.get(&format!("/v1/tasks/{allowed}/recipients")).await;
        assert!(theirs["items"].as_array().expect("a list").is_empty());
    }

    // ------------------------------------------------------------------ settings --

    #[tokio::test]
    async fn a_setting_errand_does_not_keep_is_refused_by_name() {
        let api = testkit::start().await;
        let (code, body) = api
            .post(
                "/v1/channels/telegram/config",
                json!({ "settings": { "messaging.quiett": { "from": 22, "to": 7 } } }),
            )
            .await;
        assert_eq!(code, 400, "{body}");
        let detail = body["detail"].as_str().unwrap_or_default();
        assert!(detail.contains("messaging.quiett"), "{detail}");
        assert!(
            detail.contains("messaging.quiet"),
            "the refusal must name the ones that do exist: {detail}"
        );
    }

    #[tokio::test]
    async fn quiet_hours_are_stored_in_the_shape_the_outbox_actually_reads() {
        let api = testkit::start().await;
        let (code, body) = api
            .post(
                "/v1/channels/telegram/config",
                json!({
                    "settings": {
                        "messaging.quiet": { "from": 22, "to": 7, "failures_break_through": false }
                    }
                }),
            )
            .await;
        assert_eq!(code, 200, "{body}");

        // Read back through the same key the outbox uses. Anything else is a
        // setting the person changed and nothing acts on.
        let stored = errand_core::db::get_setting(&api.pool, "messaging.quiet")
            .await
            .expect("reading it back")
            .expect("it was saved");
        assert_eq!(stored["from"], 22);
        assert_eq!(stored["to"], 7);
        assert_eq!(
            stored[super::QUIET_BREAKS_THROUGH],
            false,
            "the outbox looks for this exact word; anything else is ignored"
        );

        // And the settings screen, which asks by the other name, sees the same.
        let shown = api.get("/v1/settings").await;
        assert_eq!(
            shown["messaging.quiet"][super::QUIET_BREAKS_THROUGH_ALT],
            false
        );
    }

    #[tokio::test]
    async fn an_hour_that_is_not_an_hour_is_refused() {
        let api = testkit::start().await;
        for bad in [
            json!({ "from": 99, "to": 6 }),
            json!({ "from": "ten", "to": 6 }),
        ] {
            let (code, body) = api
                .post(
                    "/v1/channels/telegram/config",
                    json!({ "settings": { "messaging.quiet": bad } }),
                )
                .await;
            assert_eq!(code, 400, "{body}");
            assert!(
                body["detail"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("0 to 23"),
                "say what an hour looks like: {body}"
            );
        }
        assert!(
            errand_core::db::get_setting(&api.pool, "messaging.quiet")
                .await
                .expect("reading it back")
                .is_none(),
            "a refused setting must not be half-written"
        );
    }

    #[tokio::test]
    async fn a_gateway_address_that_is_not_an_address_is_refused() {
        let api = testkit::start().await;
        let (code, body) = api
            .post(
                "/v1/channels/whatsapp/config",
                json!({ "settings": { "messaging.whatsapp.base_url": "my gateway" } }),
            )
            .await;
        assert_eq!(code, 400, "{body}");

        // A real one is kept without its trailing slash, because the sender
        // builds addresses as {base}/sessions.
        let (code, body) = api
            .post(
                "/v1/channels/whatsapp/config",
                json!({ "settings": { "messaging.whatsapp.base_url": "http://localhost:3000/" } }),
            )
            .await;
        assert_eq!(code, 200, "{body}");
        assert_eq!(
            api.get("/v1/settings").await["messaging.whatsapp.base_url"],
            "http://localhost:3000"
        );
    }

    #[tokio::test]
    async fn a_task_can_be_given_its_notify_and_limits_when_it_is_created() {
        let api = testkit::start().await;
        let id = a_task(
            &api,
            json!({
                "name": "Quiet one",
                "description": "d",
                "notify": { "on_success": false, "on_failure": true },
                "limits": { "max_steps": 10, "max_minutes": 5, "max_usd": 0.10,
                            "max_heal_cycles": 1, "max_messages": 1 }
            }),
        )
        .await;
        let task = api.get(&format!("/v1/tasks/{id}")).await;
        assert_eq!(task["notify"]["on_success"], false);
        assert_eq!(task["limits"]["max_steps"], 10);
    }

    #[tokio::test]
    async fn a_task_with_nothing_filled_in_still_says_what_its_schedule_means() {
        // The dullest possible check, and the one that catches a page that
        // cannot render a task somebody has only just started.
        let api = testkit::start().await;
        let id = a_task(&api, json!({ "name": "Bare", "description": "d" })).await;
        let task = api.get(&format!("/v1/tasks/{id}")).await;
        assert_eq!(task["id"], json!(id));
        assert!(
            task["schedule_describes"]
                .as_str()
                .unwrap_or_default()
                .contains("will not run on its own"),
            "a brand new task must say plainly that nothing will happen yet: {task}"
        );
        assert!(task["schedule_preview"]
            .as_array()
            .expect("a preview list")
            .is_empty());
    }
}

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
        .route("/v1/answer-copies/{id}/open", post(open_answer_copy))
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
        .route("/v1/artifacts/{id}", get(get_artifact_file))
        .route("/v1/events", get(super::sse::global_stream))
        .route(
            "/v1/credentials",
            get(list_credentials).post(create_credential),
        )
        .route("/v1/credentials/{id}", delete(delete_credential))
        .route(
            "/v1/tasks/{id}/credentials",
            get(list_task_credentials).post(grant_credential),
        )
        .route(
            "/v1/tasks/{id}/credentials/{credential_id}",
            delete(revoke_credential),
        )
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
        .route(
            "/v1/tasks/{id}/mail",
            get(get_mail_grant).post(grant_mail).delete(revoke_mail),
        )
        .route("/v1/settings", get(get_settings))
        .route("/v1/channels", get(list_channels))
        .route("/v1/channels/{channel}/config", post(configure_channel))
        .route("/v1/channels/{channel}/test", post(test_channel))
        .route("/v1/channels/{channel}/enable", post(enable_channel))
        .route("/v1/automation", get(list_automation))
        .route("/v1/automation/{app}/enable", post(enable_automation))
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
        .route("/v1/ai/roles/{role}/model", post(set_role_model))
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
/// Enough to see the pattern (three mornings in a row, or the same day next
/// week) without turning a task page into a diary.
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
fn task_json(
    task: &errand_core::models::Task,
    open_holds: i64,
    last_run: Option<&errand_core::models::Run>,
) -> serde_json::Value {
    let mut v = serde_json::to_value(task).unwrap_or_else(|_| json!({}));
    // The newest run, so a screen can say what really happened rather than the
    // word stored on the task. A task sits at "teaching" until somebody
    // approves what it learned, so without this the list says "Learning" long
    // after the run finished, which reads as nothing having happened at all.
    v["last_run"] = match last_run {
        Some(r) => json!({
            "id": r.id,
            "status": r.status,
            "mode": r.mode,
            // Said separately from the mode, because a teach run can be a
            // rehearsal too and a screen reading only the mode would call it a
            // run that did things for real.
            "rehearsal": r.rehearsal,
            "trigger": r.trigger,
            "created_at": r.created_at,
            "finished_at": r.finished_at,
            "summary": r.summary,
            // What it produced. Sent even when null, because "this run recorded
            // no answer" is a fact the screen has to be able to tell from an
            // older build that never had the field.
            "answer": r.answer,
            "failure": r.failure,
        }),
        None => serde_json::Value::Null,
    };
    // The number the "needs you" card keys off. It is a count rather than a
    // sentence because a sentence can be reworded; an armed fence cannot.
    v["open_holds"] = json!(open_holds);
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
    let holds = errand_core::db::open_hold_counts(state.pool())
        .await
        .map_err(ApiError::from)?;
    let latest = errand_core::db::latest_run_per_task(state.pool())
        .await
        .map_err(ApiError::from)?;
    let items: Vec<serde_json::Value> = tasks
        .iter()
        .map(|t| task_json(t, *holds.get(&t.id).unwrap_or(&0), latest.get(&t.id)))
        .collect();
    Ok(Json(json!({ "items": items })))
}

async fn get_task(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Read)?;
    let task = errand_core::db::get_task(state.pool(), &id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found(format!("No task with id {id}.")))?;
    let holds = errand_core::db::count_open_holds(state.pool(), &id)
        .await
        .map_err(ApiError::from)?;
    let latest = errand_core::db::latest_run_per_task(state.pool())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(task_json(&task, holds, latest.get(&id))))
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
    let holds = errand_core::db::count_open_holds(state.pool(), &task.id)
        .await
        .map_err(ApiError::from)?;
    let latest = errand_core::db::latest_run_per_task(state.pool())
        .await
        .map_err(ApiError::from)?;
    let mut out = task_json(&task, holds, latest.get(&task.id));
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
    // `model_id` is deliberately not here. Its three answers cannot be told
    // apart once a body has been parsed into this: leaving it out, sending it
    // as null and sending an id all have to mean different things, and only the
    // body as it arrived still knows which one was sent. See below.
}

/// What an edit says about the model that carries this task out.
///
/// Read from the body as it arrived rather than from `PatchTask`, because the
/// distinction that matters here is between a field that is absent and a field
/// that is null. Absent means the edit is about something else and the task
/// keeps the model it was given: without that, saving a task's sites would
/// forget which model it was told to use. Null, or the empty string an HTML
/// menu sends for its first option, means "go back to the default".
async fn read_model_choice(
    state: &AppState,
    raw: &serde_json::Value,
) -> ApiResult<errand_core::providers::ModelChoice> {
    use errand_core::providers::ModelChoice;

    let named = match raw.get("model_id") {
        None => return Ok(ModelChoice::Unchanged),
        Some(serde_json::Value::Null) => return Ok(ModelChoice::Default),
        Some(serde_json::Value::String(s)) if s.trim().is_empty() => {
            return Ok(ModelChoice::Default)
        }
        Some(serde_json::Value::String(s)) => s.trim().to_string(),
        Some(_) => {
            return Err(ApiError::bad_request(
                "The model for a task is named by its id, as text. Nothing was changed.",
            ))
        }
    };

    // Refused now rather than saved and used later, the same way choosing a
    // model on the AI screen is. A choice that could never work is worse than
    // no choice: it fails at the hour the task was meant to happen, with
    // nobody watching.
    let providers = errand_core::db::list_providers(state.pool())
        .await
        .map_err(ApiError::from)?;
    let p = providers
        .iter()
        .find(|p| p.id == named)
        .ok_or_else(|| ApiError::not_found("There is no such model in the list."))?;
    if let Some(why) = p.cannot_carry_out_tasks(p.tools(&tools_seen(state).await)) {
        return Err(ApiError::bad_request(why));
    }
    Ok(ModelChoice::Named(named))
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
            "A task needs a description: it is what the agent actually reads, so nothing was \
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

    let model = read_model_choice(&state, &raw).await?;

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
            model,
        },
    )
    .await
    .map_err(ApiError::from)?;

    state.emit(Event::TaskUpdated {
        task_id: id.clone(),
    });

    let holds = errand_core::db::count_open_holds(state.pool(), &id)
        .await
        .map_err(ApiError::from)?;
    let latest = errand_core::db::latest_run_per_task(state.pool())
        .await
        .map_err(ApiError::from)?;
    let out = json!({
        "task": task_json(&updated, holds, latest.get(&updated.id)),
        "warnings": warnings,
    });
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
             already done by the exact time the run was due, so that run would look like fresh \
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
    // countdown jump the first time the scheduler ticked, by up to the whole
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
///
/// The four `messaging.self.*` keys hold your own address on each channel, which
/// is where a test message goes and the only place it can go. They are spelled
/// out rather than built, because a const cannot call a function; the test named
/// after this keeps them the same as the key the test button reads.
const ALLOWED_SETTINGS: &[&str] = &[
    "messaging.quiet",
    "messaging.whatsapp.base_url",
    "messaging.self.telegram",
    "messaging.self.whatsapp",
    "messaging.self.apple_mail",
    "messaging.self.imessage",
];

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
    } else if let Some(id) = self_address_channel(key) {
        Ok((check_self_address(id, value)?, None))
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
                 whole hours of the day from 0 to 23, such as 22 and 7 for an ordinary night."
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

/// Which channel a key holds your own address for, if it holds one at all.
fn self_address_channel(key: &str) -> Option<crate::channels::ChannelId> {
    crate::channels::ChannelId::parse(key.strip_prefix("messaging.self.")?)
}

/// Your own address on one channel: the only place a test message can go.
///
/// Checked as it is typed, for the same reason a contact's address is. A wrong
/// one costs ten seconds now; found at send time it costs the test, and the
/// person is left believing the channel itself is broken.
fn check_self_address(
    id: crate::channels::ChannelId,
    v: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let name = id.display_name();
    let Some(raw) = v.as_str() else {
        return Err(format!(
            "Your own {name} address has to be the address itself, as text. {} Nothing has been \
             saved.",
            self_address_shape(id)
        ));
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!(
            "Your own {name} address is empty, so a test would have nowhere to go. {} Nothing has \
             been saved.",
            self_address_shape(id)
        ));
    }
    check_address(id.as_str(), trimmed)?;
    Ok(json!(trimmed))
}

/// What the right answer looks like, said in one place because two people meet
/// it: whoever types a bad one, and whoever presses Send test having typed none.
fn self_address_shape(id: crate::channels::ChannelId) -> &'static str {
    match id {
        crate::channels::ChannelId::Telegram => {
            "It is your chat id with the bot: a number such as 123456789. Send the bot a message \
             once and it will tell you the id."
        }
        crate::channels::ChannelId::Whatsapp => {
            "It is your own phone number, with its country code, like +1 555 0100."
        }
        crate::channels::ChannelId::AppleMail => {
            "It is an email address you read, like you@example.com."
        }
        crate::channels::ChannelId::Imessage => {
            "It is your own phone number with its country code, like +1 555 0100, or the email \
             address your Apple ID uses."
        }
    }
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

/// Which logins this task may use.
async fn list_task_credentials(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Manage)?;
    let mut creds = errand_core::db::credentials_for_task(state.pool(), &id)
        .await
        .map_err(ApiError::from)?;
    // Usernames are a map of which accounts exist on which sites, so they follow
    // the same rule as the full list: admin only.
    if !caller.has(Scope::Admin) {
        for c in &mut creds {
            c.username = None;
        }
    }
    Ok(Json(json!({ "items": creds })))
}

#[derive(Deserialize)]
struct GrantCredential {
    credential_id: String,
}

/// Let one task use one saved login.
///
/// Approve rather than Manage, for the same reason granting a recipient is:
/// this decides whether an unattended agent may sign in as you somewhere. Until
/// it is granted the agent cannot see the login at all, which is why saving a
/// login on its own does nothing: a credential is stored once and then handed
/// to tasks one at a time.
async fn grant_credential(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Json(body): Json<GrantCredential>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Approve)?;

    errand_core::db::get_task(state.pool(), &id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found(format!("No task with id {id}.")))?;

    let known = errand_core::db::list_credentials(state.pool())
        .await
        .map_err(ApiError::from)?;
    let Some(cred) = known.into_iter().find(|c| c.id == body.credential_id) else {
        return Err(ApiError::not_found("There is no such saved login."));
    };

    errand_core::db::link_task_credential(state.pool(), &id, &body.credential_id)
        .await
        .map_err(ApiError::from)?;

    Ok(Json(json!({
        "granted": true,
        "label": cred.label,
        "domain": cred.domain,
        "note": format!(
            "This task may now sign in with '{}', and only on {}. It still cannot see the secret \
             itself: it asks for it to be typed into the page.",
            cred.label, cred.domain
        ),
    })))
}

async fn revoke_credential(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path((id, credential_id)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Manage)?;
    let gone = errand_core::db::unlink_task_credential(state.pool(), &id, &credential_id)
        .await
        .map_err(ApiError::from)?;
    if !gone {
        return Err(ApiError::not_found("That task was not using that login."));
    }
    Ok(Json(json!({ "revoked": true })))
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
             when it works, when it fails, or both, or remove them from the task instead.",
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

// ------------------------------------------------------------- mail access --
//
// The grant that lets one task read the person's mail, and the sentence that
// tells them where that mail then goes.
//
// The sentence lives here rather than in the interface because it is the part
// that must not drift. A screen can be redesigned by somebody who never reads
// this file; what a person is told before they hand over their post cannot be
// left to that. The interface shows what this returns.

/// Where the mail actually goes, said plainly, for the Mac as it is set up now.
///
/// Errand's whole claim is that it tells the truth about what it does, and this
/// is where that claim is tested. Whichever model carries out the task reads
/// what the mail tools return, so with a hosted model a person's private post
/// leaves their machine. Saying it softly, or only in the documentation, would
/// be the same as not saying it.
fn mail_privacy_note(local_only: bool) -> String {
    if local_only {
        return "Errand is set to keep everything on this machine, so a task reading your mail \
                sends it to a model on your own Mac or your own network and nowhere else. If you \
                ever turn that setting off, the sender, the subject and the contents of every \
                message this task opens would go to whichever service is doing the job."
            .to_string();
    }
    "Errand is set to use a model over the internet, and the model doing the job is what reads \
     your mail. So the sender, the subject and the whole of any message this task opens leave \
     this Mac and go to that service, the same as if you had pasted them into it yourself. If \
     you would rather that never happened, open the AI page, turn on \"Keep everything on this \
     machine\", and pick a model that runs on your own machine: your mail is then read here and \
     goes no further."
        .to_string()
}

/// What this task may do with the mail, and what that means.
async fn get_mail_grant(
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
    let grant = errand_core::db::mail_grant_for_task(state.pool(), &id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({
        "task_id": id,
        "granted": grant.is_some(),
        "may_file": grant.as_ref().is_some_and(|g| g.may_file),
        "granted_at": grant.as_ref().map(|g| g.granted_at.clone()),
        "local_only": local_only_setting(&state).await,
        "where_it_goes": mail_privacy_note(local_only_setting(&state).await),
    })))
}

#[derive(Deserialize)]
struct GrantMail {
    /// Off unless it is asked for. Being allowed to read somebody's mail is not
    /// the same as being allowed to rearrange it, and defaulting this to true
    /// would quietly make it the same.
    #[serde(default)]
    may_file: bool,
}

/// Let one task read the person's mail.
///
/// Approve rather than Manage, for the reason granting a recipient is: this
/// decides what an unattended agent may see of somebody's private life, which
/// is the same class of decision as resolving a hold. A client that can change
/// settings should not be able to make it.
async fn grant_mail(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Json(body): Json<GrantMail>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Approve)?;

    if errand_core::db::get_task(state.pool(), &id)
        .await
        .map_err(ApiError::from)?
        .is_none()
    {
        return Err(ApiError::not_found(format!("No task with id {id}.")));
    }

    errand_core::db::grant_mail(state.pool(), &id, body.may_file)
        .await
        .map_err(ApiError::from)?;

    state.emit(Event::TaskUpdated {
        task_id: id.clone(),
    });
    let local_only = local_only_setting(&state).await;
    Ok(Json(json!({
        "task_id": id,
        "granted": true,
        "may_file": body.may_file,
        "local_only": local_only,
        "where_it_goes": mail_privacy_note(local_only),
        "note": if body.may_file {
            "This task can now read your mail and move messages between mailboxes. Every message \
             it opens and every one it moves is written down in the run, by sender and subject, \
             so you can see afterwards exactly what it touched."
        } else {
            "This task can now read your mail. It cannot move, delete or send anything, and \
             every message it opens is written down in the run, by sender and subject, so you \
             can see afterwards exactly what it read."
        }
    })))
}

/// Take the mail away from one task.
async fn revoke_mail(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    // Manage is enough to take a permission away. Removing this can only ever
    // mean a task seeing less.
    require(&caller, Scope::Manage)?;
    let gone = errand_core::db::revoke_mail(state.pool(), &id)
        .await
        .map_err(ApiError::from)?;
    if !gone {
        return Err(ApiError::not_found(
            "This task could not see your mail anyway, so nothing changed.",
        ));
    }
    state.emit(Event::TaskUpdated {
        task_id: id.clone(),
    });
    Ok(Json(json!({
        "task_id": id,
        "granted": false,
        "note": "This task can no longer see your mail: from its next run onwards the tools for \
                 reading it are not even offered to it. Anything it has already read or moved is \
                 not undone by this, and what it read is in the runs."
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
                 start a conversation from one: message the bot once and it will tell you the id."
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
                           press Enable, so the prompt appears while you are looking at it. \
                           Reading your post is a separate card, under Apps on this Mac.",
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

    // Only ever to you, and only ever to the address you saved for yourself.
    // Nothing here reads the request body: a test that could name a recipient
    // would be a convenient way to make Errand send anything to anyone.
    let mut recipient = crate::channels::self_address(state.pool(), id).await;
    if recipient.is_none() && id == crate::channels::ChannelId::Telegram {
        // Anyone with Telegram already working set a chat id long before this
        // box existed, and should not have to type the same thing twice.
        recipient = crate::channels::telegram::configured_chat_id().await;
    }
    let Some(recipient) = recipient else {
        let name = id.display_name();
        return Err(ApiError::conflict(
            "no_self_recipient",
            format!(
                "A test only ever goes to you, and Errand does not know your own {name} address \
                 yet. Fill in your own address for {name} on the settings screen, which is the \
                 setting {key}, then press Send test again. {shape}",
                key = crate::channels::self_address_key(id),
                shape = self_address_shape(id),
            ),
        ));
    };

    let recipient_shown = recipient.clone();
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
        // Said back, and the screen puts it on the button too. A test sends a
        // real message to a real device: firing one without knowing where it
        // is going is how somebody messages a number they typed to try the
        // feature out.
        "sent_to": recipient_shown,
        "note": format!(
            "Queued for {recipient_shown}. It goes out within a few seconds; check this channel \
             afterwards to see whether it really sent."
        ),
    })))
}

/// Ask macOS for Automation permission now, while somebody is watching.
async fn enable_channel(
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
    // The prompt has to come from the daemon, because macOS grants Automation
    // to the process that sends the Apple Event. Asking from anywhere else
    // grants it to the wrong thing and the 03:00 run still fails.
    let mut health = crate::channels::apple::request_consent(id).await;
    // The screen draws this the same way it draws the channel list, so it needs
    // the same fields: the name to show, and where a test would go.
    health.fill_self_address(state.pool()).await;
    Ok(Json(serde_json::to_value(health).unwrap_or(json!({}))))
}

// ------------------------------------------ apps on this Mac, not channels --

/// Which apps on this Mac Errand may drive, and what to do when it may not.
///
/// Asking macOS is the same act as checking, so loading this is itself what
/// puts the prompt on the screen. That is the point of doing it here: the
/// person is looking at the screen now, and at 03:00 they will not be.
async fn list_automation(
    Extension(caller): Extension<Caller>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Read)?;
    Ok(Json(json!({
        "apps": crate::channels::apple::all_app_consent().await,
        "notes": {
            "mail_reading":
                "Sending your post and reading it are one macOS permission, because macOS decides \
                 per app rather than per thing you do with it, so turning either on normally \
                 turns both on. Errand checks each on its own rather than assuming that, so \
                 believe what each line says over this one.",
            "notes":
                "Lets a task write into Apple Notes when you ask it for a note. The answer \
                 itself always appears on the task, with or without this.",
        }
    })))
}

/// Ask macOS for permission to drive one app, while somebody is watching.
async fn enable_automation(
    Extension(caller): Extension<Caller>,
    Path(app): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Manage)?;
    let Some(app) = crate::channels::apple::Automation::parse(&app) else {
        return Err(ApiError::bad_request(format!(
            "'{app}' is not an app Errand drives."
        )));
    };
    // The same reason as enable_channel above: macOS grants Automation to the
    // process that sends the Apple Event, so the asking has to happen in the
    // daemon. Asking from the window grants it to the window, and the 03:00 run
    // fails exactly as before.
    let health = crate::channels::apple::app_consent(app).await;
    Ok(Json(serde_json::to_value(health).unwrap_or(json!({}))))
}

/// What a teach run may be asked for. Nothing is required: teaching for real is
/// what this endpoint has always done with no body at all, and callers written
/// before rehearsing existed must keep working exactly as they did.
#[derive(Deserialize, Default)]
struct TeachBody {
    #[serde(default)]
    dry_run: bool,
}

/// Start the supervised first run of a task.
///
/// A teach run is how a task learns. It works from the description alone and
/// writes down what actually worked, which a person then approves.
///
/// `{"dry_run": true}` teaches it as a rehearsal: the agent works the job out
/// and writes its plan exactly as it would otherwise, and everything
/// irreversible is recorded instead of done. That matters here more than
/// anywhere, because a task may not run until it has been taught, so without
/// this the first run of "clean my mailbox of spam" really moves the post.
async fn teach_task(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    body: Option<Json<TeachBody>>,
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

    let rehearsing = body.map(|b| b.0.dry_run).unwrap_or(false);
    let occurrence = format!("teach/{}", errand_core::new_id());
    let run = errand_core::db::try_create_run(
        state.pool(),
        &id,
        &occurrence,
        "teach",
        errand_core::models::RunMode::teach(rehearsing),
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
            "Teaching '{}': working it out from the description{}",
            task.name,
            if rehearsing {
                ", as a rehearsal, so nothing that cannot be undone will really happen"
            } else {
                ""
            }
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
        // Every version carries its own text, approved or not.
        //
        // The note below tells a person to read what the task wrote and then
        // approve it. Until now only the approved version came back with any
        // text in it, so the one thing they were being asked to read was the
        // one thing they could not see -- which turns the approval gate, the
        // single line between "tried once while somebody watched" and "runs
        // alone at three in the morning", into a button people press blind.
        "versions": versions.iter().map(|v| json!({
            "version": v.version,
            "source": v.source,
            "approved": v.approved,
            "changelog": v.changelog,
            "created_by_run_id": v.created_by_run_id,
            "created_at": v.created_at,
            "sha256": v.sha256,
            "markdown": errand_core::playbook::read(&id, v.version)
                .ok()
                .map(|p| p.to_markdown()),
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
        errand_core::models::RunMode::run(dry),
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
    let copies = errand_core::db::list_answer_copies(state.pool(), &id)
        .await
        .map_err(ApiError::from)?;
    let mut body = serde_json::to_value(&run).unwrap_or(json!({}));
    body["steps"] = serde_json::to_value(steps).unwrap_or(json!([]));
    // Where else this run put its answer, if the task asked for a copy. The
    // answer itself is above; these are the extra places it also went.
    body["answer_copies"] = serde_json::to_value(copies).unwrap_or(json!([]));
    Ok(Json(body))
}

/// Show the person one of the places a run left a copy of its answer.
///
/// The id is the whole request. What gets opened comes from the row, never from
/// the caller, so this cannot be talked into opening a file somebody names.
async fn open_answer_copy(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Read)?;
    let copy = errand_core::db::get_answer_copy(state.pool(), &id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found("No copy with that id."))?;

    let opened = match copy.kind.as_str() {
        "note" => crate::desktop::open_note(&copy.locator).await,
        "file" => crate::desktop::open_file(std::path::Path::new(&copy.locator)).await,
        other => {
            return Err(ApiError::bad_request(format!(
                "A {other} is not something Errand can open for you."
            )))
        }
    };
    opened.map_err(|e| ApiError::from(anyhow::anyhow!(e)))?;
    Ok(Json(json!({ "opened": copy.label })))
}

/// One file a run left behind, such as a screenshot, served by id.
///
/// The id is the whole story: the row decides the path, never the request,
/// and the path then has to prove it sits under the data root before a byte
/// is read. An artifact id a token cannot see simply does not exist.
async fn get_artifact_file(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<axum::response::Response> {
    use axum::response::IntoResponse;

    require(&caller, Scope::Read)?;
    let artifact = errand_core::db::get_artifact(state.pool(), &id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found("No artifact with that id."))?;

    let root = errand_core::paths::data_root().map_err(ApiError::from)?;
    let canon_root =
        std::fs::canonicalize(&root).map_err(|e| ApiError::from(anyhow::anyhow!(e)))?;
    let canon = std::fs::canonicalize(root.join(&artifact.rel_path))
        .map_err(|_| ApiError::not_found("That artifact's file is no longer there."))?;
    if !canon.starts_with(&canon_root) {
        return Err(ApiError::not_found("No artifact with that id."));
    }

    let bytes = tokio::fs::read(&canon)
        .await
        .map_err(|_| ApiError::not_found("That artifact's file is no longer there."))?;
    let mime = match artifact.kind.as_str() {
        "screenshot" => "image/png",
        _ => "application/octet-stream",
    };
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, mime),
            (axum::http::header::CACHE_CONTROL, "private, max-age=3600"),
        ],
        bytes,
    )
        .into_response())
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
// loud, so all of it (what is configured, what is reachable, and what each job
// would actually use right now) comes back from one call.

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
    let seen = tools_seen(&state).await;
    let claude = crate::models::claude_models(&state).await;

    // Each model with what Errand has found out about it, because "can this one
    // do the task" is the question the screen exists to answer and it must not
    // be guessed at from the kind of endpoint.
    let listed: Vec<serde_json::Value> = providers
        .iter()
        .map(|p| {
            let tools = p.tools(&seen);
            json!({
                "id": p.id,
                "kind": p.kind,
                "label": p.label,
                "base_url": p.base_url,
                "model": p.model,
                // The command line tool has no model in its row: it answers to
                // three, one per job. Without this the screen says "Claude" and
                // stops, which is exactly the question nobody could answer.
                "models_in_use": is_claude_cli(p)
                    .then(|| errand_core::providers::claude_models_summary(&claude)),
                "enabled": p.enabled,
                "discovered": p.discovered,
                "health": p.health,
                "health_detail": p.health_detail,
                "tools": tools.as_str(),
                "tools_says": tools.describe(),
                // Null when it can, or when nobody has checked. Only a model
                // Errand has actually found wanting gets a reason.
                "cannot_carry_out_because": p.cannot_carry_out_tasks(tools),
            })
        })
        .collect();

    // For each job, what would happen if it were asked right now. This is the
    // part that turns "it uses AI somehow" into something you can point at.
    let roles: Vec<serde_json::Value> = errand_core::providers::Role::ALL
        .iter()
        .map(|&role| {
            let chain = errand_core::providers::resolve_chain_knowing(
                &providers, &bindings, role, local_only, &seen,
            );
            let chosen = bindings
                .iter()
                .find(|(r, _)| *r == role)
                .map(|(_, id)| id.clone());
            // A model can be picked for a job and only later turn out to be
            // unable to do it, in which case the chain quietly moves on and the
            // person is owed the reason rather than a silent substitution.
            let chosen_problem = chosen.as_ref().and_then(|id| {
                providers
                    .iter()
                    .find(|p| &p.id == id)
                    .and_then(|p| p.cannot_fill(role, p.tools(&seen)))
            });
            json!({
                "role": role.as_str(),
                "explains": role.describe(),
                "needs_agentic": role.needs_agentic(),
                // Said plainly, so the screen cannot offer a choice that would
                // not change anything.
                "in_use": role.is_wired(),
                "not_used_because": role.not_wired_reason(),
                "chosen": chosen,
                "chosen_problem": chosen_problem,
                "using": chain.first().map(|p| {
                    // A model choice is only offered where there is one to
                    // make. The rule this screen lives under is that it never
                    // shows a control that would change nothing.
                    let cli = is_claude_cli(p);
                    let model = if cli {
                        errand_core::providers::claude_model_for(role, &claude).to_string()
                    } else {
                        p.model.clone().unwrap_or_else(||
                            errand_core::providers::default_model_for(role).to_string())
                    };
                    let known = cli.then(|| errand_core::providers::claude_model(&model)).flatten();
                    json!({
                        "id": p.id,
                        "label": p.label,
                        "model": model,
                        "model_name": known.map(|m| m.name),
                        "model_says": known.map(|m| m.what_it_is_for),
                        "can_choose_model": cli,
                        "local": p.is_local(),
                    })
                }),
                "fallbacks": chain.iter().skip(1).map(|p| p.label.clone()).collect::<Vec<_>>(),
                "problem": chain.is_empty().then(||
                    errand_core::providers::explain_empty_chain(role, local_only, &providers)),
            })
        })
        .collect();

    Ok(Json(json!({
        "providers": listed,
        "roles": roles,
        // The three the command line tool answers to, so the screen offers
        // exactly what Errand will accept rather than a list of its own.
        "claude_models": errand_core::providers::CLAUDE_MODELS,
        "local_only": local_only,
    })))
}

/// Is this the Claude command line tool, the one endpoint with a model per job?
fn is_claude_cli(p: &errand_core::providers::Provider) -> bool {
    p.kind_enum() == Some(errand_core::providers::Kind::ClaudeCli)
}

// --------------------------------------------------- can it use tools? --
//
// Carrying out a task needs exactly one thing from a model: that it answers
// with a tool call instead of with a paragraph. Errand supplies the loop, the
// tools, the budget and the fence, so this is the whole question, and it is
// cheaper to ask the model once than to find out at seven in the morning when
// a booking did not happen.

/// What Errand has learned about which models can use tools.
async fn tools_seen(state: &AppState) -> errand_core::providers::ToolsSeen {
    let stored = errand_core::db::get_setting(state.pool(), errand_core::providers::TOOLS_SEEN_KEY)
        .await
        .ok()
        .flatten();
    errand_core::providers::read_tools_seen(stored.as_ref())
}

/// Write down what one model turned out to be able to do.
///
/// "Unknown" is stored by forgetting rather than by writing the word, so the
/// note holds only what was actually found out and a model that is removed
/// leaves nothing behind.
async fn remember_tools(state: &AppState, id: &str, tools: errand_core::providers::Tools) {
    let mut seen = tools_seen(state).await;
    if tools == errand_core::providers::Tools::Unknown {
        seen.remove(id);
    } else {
        seen.insert(id.to_string(), tools);
    }
    let _ = errand_core::db::set_setting(
        state.pool(),
        errand_core::providers::TOOLS_SEEN_KEY,
        &errand_core::providers::write_tools_seen(&seen),
    )
    .await;
}

/// How long to wait for an answer to the tool question.
///
/// Long enough for a large model that is not in memory yet.
///
/// Measured rather than guessed: on an ordinary home network a 27B model that
/// has to load itself from disk took about 150 seconds to answer its first
/// question, and answered in under two once it was warm. A short timeout would
/// report exactly the models somebody most wants to use as unchecked, over and
/// over, which reads as the feature not working. Waiting is the lesser evil,
/// and the screen says it may take a couple of minutes.
const TOOL_CHECK_SECONDS: u64 = 240;

/// Ask a model the smallest question that can only be answered with a tool call.
///
/// Deliberately trivial and free of side effects. It proves one thing, that the
/// model will reach for a tool when it is given one, and claims nothing at all
/// about how well it would do a real errand.
///
/// Returns what was found out, and a sentence for the health detail saying how
/// it was found out, because a verdict with no evidence behind it is the sort of
/// thing people rightly distrust.
async fn ask_whether_it_can_use_tools(
    p: &errand_core::providers::Provider,
) -> (errand_core::providers::Tools, String) {
    use errand_core::providers::Tools;

    let base = p.base_url.clone().unwrap_or_default();
    let Some(model) = p.model.clone().filter(|m| !m.trim().is_empty()) else {
        return (
            Tools::Unknown,
            "Errand does not know which model to ask for here, so it could not find out whether \
             this one can carry out tasks. Give it a model name and check again."
                .into(),
        );
    };

    let tools = vec![json!({
        "type": "function",
        "function": {
            "name": "report",
            "description": "The only way to reply. Call this with the answer.",
            "parameters": {
                "type": "object",
                "properties": {
                    "answer": { "type": "string", "description": "The answer to the question." }
                },
                "required": ["answer"],
            }
        }
    })];
    let messages = vec![
        json!({
            "role": "system",
            "content": "You answer only by calling a tool. Never reply in words.",
        }),
        json!({
            "role": "user",
            "content": "What colour is a ripe lemon? Reply by calling the report tool.",
        }),
    ];

    let key = crate::models::key_for(&p.id).await;
    let asked = tokio::time::timeout(
        std::time::Duration::from_secs(TOOL_CHECK_SECONDS),
        crate::models::chat_with_tools(&base, &model, &messages, &tools, key.as_deref()),
    )
    .await;

    match asked {
        Ok(Ok(turn)) if !turn.tool_calls.is_empty() => (
            Tools::Yes,
            format!("{model} used the tool it was given, so it can carry out tasks."),
        ),
        Ok(Ok(_)) => (
            Tools::No,
            format!(
                "{model} answered in words instead of using the tool it was given, so it cannot \
                 drive a browser. It can still do Errand's other three jobs."
            ),
        ),
        Ok(Err(e)) if e.no_tool_support => (
            Tools::No,
            format!(
                "{model} does not support tool calling, so it cannot carry out a task. It can \
                 still do Errand's other three jobs. What the server said: {e}"
            ),
        ),
        Ok(Err(e)) => (
            Tools::Unknown,
            format!("Errand could not find out whether {model} can use tools: {e}"),
        ),
        Err(_) => (
            Tools::Unknown,
            format!(
                "{model} did not answer within {TOOL_CHECK_SECONDS} seconds, so Errand still does \
                 not know whether it can carry out tasks. A model that has just been loaded is \
                 often slow the first time: press Check again."
            ),
        ),
    }
}

/// Why a provider is being checked, which decides what to do about the tool
/// question.
enum Checking {
    /// Where this model lives, or which model it is, has just changed. Whatever
    /// was known is about something else now, so it is dropped rather than
    /// carried over onto a model that never earned it.
    AfterAChange,
    /// Somebody pressed Check. Ask again, and keep what was already known if
    /// the question cannot be put, because a machine being asleep is not an
    /// answer about what it can do.
    OnPurpose,
    /// Only the switch was flipped. Nothing new to find out, and nobody should
    /// be made to wait on a model while they turn it off.
    NothingNew,
}

/// Check a provider, and where it makes sense, find out whether it can be
/// handed a whole task.
async fn check_and_remember(
    state: &AppState,
    p: &errand_core::providers::Provider,
    why: Checking,
) -> (&'static str, String) {
    use errand_core::providers::{Kind, Tools};

    let (status, mut detail) = crate::models::check_one(p).await;

    // Only the OpenAI-compatible ones are a question. The command line tool is
    // an agent loop already, and Anthropic's own API is refused for a reason
    // that has nothing to do with what its model can do.
    if !matches!(p.kind_enum(), Some(Kind::OpenAiCompat)) || matches!(why, Checking::NothingNew) {
        return (status, detail);
    }

    if status == "ok" {
        let (tools, said) = ask_whether_it_can_use_tools(p).await;
        detail = format!("{detail} {said}");
        if tools != Tools::Unknown || matches!(why, Checking::AfterAChange) {
            remember_tools(state, &p.id, tools).await;
        }
    } else if matches!(why, Checking::AfterAChange) {
        remember_tools(state, &p.id, Tools::Unknown).await;
    }

    (status, detail)
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

    // What this row said before, so that flipping the switch on a model does not
    // put somebody through the whole interrogation again.
    let before = errand_core::db::list_providers(state.pool())
        .await
        .map_err(ApiError::from)?
        .into_iter()
        .find(|e| e.id == provider.id);
    let why = match &before {
        Some(b)
            if b.base_url == provider.base_url
                && b.model == provider.model
                && body.key.is_none() =>
        {
            Checking::NothingNew
        }
        _ => Checking::AfterAChange,
    };

    errand_core::db::upsert_provider(state.pool(), &provider)
        .await
        .map_err(ApiError::from)?;

    // Asked now, once, rather than assumed: a model is capable or not on its
    // own merits, and the answer belongs on the screen before anybody relies
    // on it for a real errand.
    let (status, detail) = check_and_remember(&state, &provider, why).await;
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
    // And so does what Errand learned about it, or a model added later at the
    // same address would inherit a verdict it never earned.
    remember_tools(&state, &id, errand_core::providers::Tools::Unknown).await;
    Ok(Json(json!({ "removed": true })))
}

/// Ask a provider whether it is really there and what it can do, then say what
/// came back.
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

    let (status, detail) = check_and_remember(&state, &p, Checking::OnPurpose).await;
    errand_core::db::set_provider_health(state.pool(), &id, status, Some(&detail))
        .await
        .map_err(ApiError::from)?;

    let tools = p.tools(&tools_seen(&state).await);
    Ok(Json(json!({
        "health": status,
        "health_detail": detail,
        "tools": tools.as_str(),
        "tools_says": tools.describe(),
    })))
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
        // Only a model Errand has actually found wanting is refused. One nobody
        // has checked is allowed: refusing it would be inventing a limitation.
        if role.needs_agentic() {
            let tools = p.tools(&tools_seen(&state).await);
            if let Some(why) = p.cannot_carry_out_tasks(tools) {
                return Err(ApiError::bad_request(why));
            }
        }
    }

    errand_core::db::set_role_binding(state.pool(), role, body.provider_id.as_deref())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct RoleModel {
    /// Missing or empty means "whichever Errand would pick", so a job can be
    /// put back on its default without anybody having to remember what it was.
    model: Option<String>,
}

/// Choose which Claude does one job.
///
/// Only the command line tool has a choice here: everything else is one
/// endpoint with one model, chosen where it was added.
async fn set_role_model(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(role): Path<String>,
    Json(body): Json<RoleModel>,
) -> ApiResult<Json<serde_json::Value>> {
    require(&caller, Scope::Manage)?;
    let role = errand_core::providers::Role::parse(&role)
        .ok_or_else(|| ApiError::bad_request(format!("'{role}' is not one of Errand's jobs.")))?;

    let mut chosen = crate::models::claude_models(&state).await;
    match body
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        Some(alias) => {
            // Refused now rather than saved and used later: a name the tool
            // does not know fails the run at the hour the task was meant to
            // happen, with nobody watching.
            let m = errand_core::providers::claude_model(alias).ok_or_else(|| {
                ApiError::bad_request(format!(
                    "Errand can ask the Claude command line tool for Opus, Sonnet or Haiku. It \
                     does not know '{alias}'."
                ))
            })?;
            chosen.insert(role.as_str().to_string(), m.alias.to_string());
        }
        None => {
            chosen.remove(role.as_str());
        }
    }

    errand_core::db::set_setting(
        state.pool(),
        errand_core::providers::CLAUDE_MODELS_KEY,
        &errand_core::providers::write_claude_models(&chosen),
    )
    .await
    .map_err(ApiError::from)?;

    Ok(Json(json!({
        "role": role.as_str(),
        "model": errand_core::providers::claude_model_for(role, &chosen),
    })))
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
                 stop everything. Add one first: Find models on this machine will look.",
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
    use serde_json::{json, Value};

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

    // ------------------------------------------------ teaching it as a rehearsal --

    /// The first run of a task is always a teach run, because nothing else is
    /// allowed until a plan is approved. So this is the only way to watch a
    /// task that books, sends or moves things without any of it happening.
    #[tokio::test]
    async fn a_task_can_be_taught_as_a_rehearsal() {
        let api = testkit::start().await;
        let id = a_task(
            &api,
            json!({ "name": "Tidy the inbox", "description": "File anything that is junk." }),
        )
        .await;

        let (code, run) = api
            .post(&format!("/v1/tasks/{id}/teach"), json!({ "dry_run": true }))
            .await;
        assert_eq!(code, 200, "{run}");
        assert_eq!(
            run["mode"], "teach",
            "a rehearsed teach is still teaching, or it would never write a plan"
        );
        assert_eq!(
            run["rehearsal"], true,
            "everything irreversible must be recorded rather than done: {run}"
        );

        let detail = api
            .get(&format!("/v1/runs/{}", run["id"].as_str().unwrap()))
            .await;
        let first = detail["steps"][0]["title"].as_str().unwrap_or_default();
        assert!(
            first.contains("rehearsal"),
            "somebody watching has to be told nothing will really happen: {first}"
        );
    }

    #[tokio::test]
    async fn teaching_a_task_the_ordinary_way_still_does_the_job_for_real() {
        let api = testkit::start().await;
        let id = a_task(
            &api,
            json!({ "name": "Tidy the inbox", "description": "File anything that is junk." }),
        )
        .await;

        // No body at all, which is how every caller written before rehearsing
        // existed asks for this.
        let (code, run) = api
            .post_with_no_body(&format!("/v1/tasks/{id}/teach"))
            .await;
        assert_eq!(code, 200, "{run}");
        assert_eq!(run["mode"], "teach");
        assert_eq!(
            run["rehearsal"], false,
            "teaching without asking for a rehearsal must do the job for real: {run}"
        );
    }

    #[tokio::test]
    async fn a_task_taught_as_a_rehearsal_still_has_to_be_taught_before_it_runs() {
        // A rehearsal writes a plan, and that plan still waits for a person.
        // Nothing about asking for a rehearsal opens the gate early.
        let api = testkit::start().await;
        let id = a_task(
            &api,
            json!({ "name": "Court", "description": "Book the usual court." }),
        )
        .await;
        let (code, body) = api
            .post(&format!("/v1/tasks/{id}/teach"), json!({ "dry_run": true }))
            .await;
        assert_eq!(code, 200, "{body}");

        let (code, body) = api.post(&format!("/v1/tasks/{id}/run"), json!({})).await;
        assert_eq!(code, 409, "{body}");
        assert_eq!(body["code"], "task_not_taught");
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
            errand_core::models::RunMode::NORMAL,
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
            errand_core::models::RunMode::NORMAL,
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

    /// Somewhere to point a task's model choice at, without a model server.
    ///
    /// Nothing is asked of it here: these tests are about the choice being
    /// remembered and offered honestly, not about a model doing any work.
    async fn a_model_in_the_list(api: &testkit::Api, id: &str, label: &str) {
        errand_core::db::upsert_provider(
            &api.pool,
            &errand_core::providers::Provider {
                id: id.into(),
                kind: "openai_compat".into(),
                label: label.into(),
                base_url: Some("http://127.0.0.1:11434/v1".into()),
                model: Some("qwen3.5-27b".into()),
                enabled: true,
                discovered: false,
                health: None,
                health_detail: None,
            },
        )
        .await
        .expect("saving a model");
    }

    #[tokio::test]
    async fn editing_a_task_s_sites_does_not_forget_which_model_it_uses() {
        // The trap this closes: the task page saves one section at a time, so
        // every save is an edit that says nothing about the model. If saying
        // nothing meant "no model", a task would lose the model somebody chose
        // for it the next time they added a site, and nothing would say so.
        let api = testkit::start().await;
        let id = a_task(
            &api,
            json!({ "name": "Post", "description": "Read the post." }),
        )
        .await;
        a_model_in_the_list(&api, "mine", "The model on my desk").await;

        let (code, body) = api
            .patch(&format!("/v1/tasks/{id}"), json!({ "model_id": "mine" }))
            .await;
        assert_eq!(code, 200, "naming a model failed: {body}");

        let (code, body) = api
            .patch(
                &format!("/v1/tasks/{id}"),
                json!({ "allowed_domains": ["example.com"] }),
            )
            .await;
        assert_eq!(code, 200, "changing the sites failed: {body}");

        assert_eq!(
            api.get(&format!("/v1/tasks/{id}")).await["model_id"],
            "mine",
            "editing the sites must leave the task's model alone"
        );
    }

    #[tokio::test]
    async fn a_task_can_be_put_back_on_whichever_model_everything_else_uses() {
        let api = testkit::start().await;
        let id = a_task(&api, json!({ "name": "Court", "description": "Book it." })).await;
        a_model_in_the_list(&api, "mine", "The model on my desk").await;

        api.patch(&format!("/v1/tasks/{id}"), json!({ "model_id": "mine" }))
            .await;
        let (code, body) = api
            .patch(&format!("/v1/tasks/{id}"), json!({ "model_id": null }))
            .await;
        assert_eq!(code, 200, "putting it back on the default failed: {body}");

        assert_eq!(
            api.get(&format!("/v1/tasks/{id}")).await["model_id"],
            Value::Null,
            "a task put back on the default must not still name a model"
        );
    }

    #[tokio::test]
    async fn a_task_cannot_be_told_to_use_a_model_that_is_not_there() {
        let api = testkit::start().await;
        let id = a_task(&api, json!({ "name": "Court", "description": "Book it." })).await;
        let (code, body) = api
            .patch(
                &format!("/v1/tasks/{id}"),
                json!({ "model_id": "one-that-was-removed" }),
            )
            .await;
        assert_eq!(code, 404, "{body}");
        assert_eq!(
            api.get(&format!("/v1/tasks/{id}")).await["model_id"],
            Value::Null
        );
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

    // ------------------------------------------------------ reading a plan --

    #[tokio::test]
    async fn a_plan_can_be_read_before_it_is_approved() {
        // The approval gate is the one line between "somebody watched it try
        // once" and "it does this alone at three in the morning", and the API
        // tells a person to read what the task wrote before crossing it. Only
        // the approved version used to come back with any text, so the plan
        // they were being asked to read was the one thing they could not see.
        let api = testkit::start().await;
        let id = a_task(
            &api,
            json!({ "name": "Tidy", "description": "Tidy the inbox." }),
        )
        .await;

        let version = errand_core::db::next_playbook_version(&api.pool, &id)
            .await
            .expect("the next version number");
        let pb = errand_core::playbook::Playbook {
            version,
            goal: "Move the daily digests to Junk.".into(),
            sites: vec![],
            preconditions: vec![],
            steps: vec![errand_core::playbook::Step {
                intent: "Look at the five most recent messages.".into(),
                hint: None,
                decision: None,
            }],
            success: vec![],
            known_failures: vec![],
            never: vec![],
        };
        errand_core::db::add_playbook_version(
            &api.pool,
            &id,
            &pb,
            errand_core::playbook::Source::Teach,
            None,
            Some("Written by a rehearsal, so nothing in it was actually done."),
            false,
        )
        .await
        .expect("storing a plan nobody has approved");

        let body = api.get(&format!("/v1/tasks/{id}/playbook")).await;
        assert!(
            body["active"].is_null(),
            "nothing should be approved yet: {body}"
        );
        let waiting = &body["versions"][0];
        assert_eq!(waiting["approved"], false);
        let text = waiting["markdown"]
            .as_str()
            .unwrap_or_else(|| panic!("a plan awaiting approval has to carry its text: {body}"));
        assert!(
            text.contains("Move the daily digests to Junk")
                && text.contains("Look at the five most recent messages"),
            "the text has to be the plan itself, not a summary of it: {text}"
        );
        // And why it should be read with care, which is the other half of
        // deciding whether to approve it.
        assert!(
            waiting["changelog"]
                .as_str()
                .is_some_and(|c| c.contains("rehearsal")),
            "a plan written by a rehearsal has to say so: {body}"
        );
    }

    // ------------------------------------------------- the mail a task may read --

    #[tokio::test]
    async fn a_task_starts_with_no_reach_into_the_mail_and_says_so() {
        let api = testkit::start().await;
        let id = a_ready_manual_task(&api).await;

        let before = api.get(&format!("/v1/tasks/{id}/mail")).await;
        assert_eq!(before["granted"], false);
        assert_eq!(before["may_file"], false);

        let (code, body) = api
            .post(
                &format!("/v1/tasks/{id}/mail"),
                json!({ "may_file": false }),
            )
            .await;
        assert_eq!(code, 200, "{body}");

        let after = api.get(&format!("/v1/tasks/{id}/mail")).await;
        assert_eq!(after["granted"], true);
        assert_eq!(
            after["may_file"], false,
            "reading the mail must not quietly bring moving it along with it"
        );

        let (code, body) = api.delete(&format!("/v1/tasks/{id}/mail")).await;
        assert_eq!(code, 200, "{body}");
        assert_eq!(
            api.get(&format!("/v1/tasks/{id}/mail")).await["granted"],
            false
        );
    }

    #[tokio::test]
    async fn the_screen_that_hands_over_your_mail_says_where_the_mail_then_goes() {
        let api = testkit::start().await;
        let id = a_ready_manual_task(&api).await;

        // As Errand comes out of the box: a model over the internet, which is
        // the case where this has to be said out loud rather than implied.
        let said = api.get(&format!("/v1/tasks/{id}/mail")).await["where_it_goes"]
            .as_str()
            .expect("a plain sentence about where the mail goes")
            .to_string();
        assert!(
            said.contains("leave this Mac"),
            "a person handing over their post has to be told it leaves the machine: {said}"
        );
        assert!(
            said.contains("Keep everything on this machine"),
            "the way out has to be named, not left to be discovered: {said}"
        );
        assert!(
            said.contains("runs on your own machine"),
            "a local model is the private alternative and has to be offered: {said}"
        );

        // And the other way round, once nothing is allowed to leave.
        errand_core::db::set_setting(&api.pool, "privacy.local_only", &json!(true))
            .await
            .expect("the setting");
        let said = api.get(&format!("/v1/tasks/{id}/mail")).await["where_it_goes"]
            .as_str()
            .expect("a plain sentence")
            .to_string();
        assert!(
            said.contains("nowhere else"),
            "with everything kept local the person should be told that plainly: {said}"
        );
        assert!(
            said.contains("If you ever turn that setting off"),
            "the promise has to name what would end it: {said}"
        );
    }

    #[tokio::test]
    async fn handing_a_task_your_mail_needs_the_permission_that_approves_things() {
        let api = testkit::start().await;
        let id = a_ready_manual_task(&api).await;

        // A token that can change settings but not approve anything.
        let settings_only = testkit::mint(&api.pool, "settings-only-mail", "read,manage").await;
        let (code, body) = api
            .as_token(
                &settings_only,
                reqwest::Method::POST,
                &format!("/v1/tasks/{id}/mail"),
                Some(json!({ "may_file": true })),
                None,
            )
            .await;
        assert_eq!(
            code, 403,
            "deciding that an unattended agent may read somebody's post is an approval: {body}"
        );

        let (code, body) = api
            .post(&format!("/v1/tasks/{id}/mail"), json!({ "may_file": true }))
            .await;
        assert_eq!(code, 200, "{body}");

        // Taking it away again is always safe, so managing is enough.
        let (code, body) = api
            .as_token(
                &settings_only,
                reqwest::Method::DELETE,
                &format!("/v1/tasks/{id}/mail"),
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
    async fn the_mail_is_granted_to_one_task_and_no_other_task_inherits_it() {
        let api = testkit::start().await;
        let allowed = a_ready_manual_task(&api).await;
        let other = a_task(&api, json!({ "name": "Other", "description": "d" })).await;

        let (code, body) = api
            .post(
                &format!("/v1/tasks/{allowed}/mail"),
                json!({ "may_file": true }),
            )
            .await;
        assert_eq!(code, 200, "{body}");

        assert_eq!(
            api.get(&format!("/v1/tasks/{other}/mail")).await["granted"],
            false,
            "a grant on one task must never reach another"
        );
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

    // ------------------------------------------- where a test message goes --

    #[test]
    fn the_key_the_settings_screen_writes_is_the_key_the_test_button_reads() {
        // Spelled out in one place and built in the other, so this is the only
        // thing keeping them the same. If they drifted, the box would fill in
        // and Send test would still say it does not know where to send.
        for id in [
            crate::channels::ChannelId::Telegram,
            crate::channels::ChannelId::Whatsapp,
            crate::channels::ChannelId::AppleMail,
            crate::channels::ChannelId::Imessage,
        ] {
            let key = crate::channels::self_address_key(id);
            assert!(
                super::ALLOWED_SETTINGS.contains(&key.as_str()),
                "{key} is read by the test button but the settings route would refuse to save it"
            );
            assert_eq!(super::self_address_channel(&key), Some(id));
        }
        assert_eq!(super::self_address_channel("messaging.self.fax"), None);
        assert_eq!(super::self_address_channel("messaging.quiet"), None);
    }

    /// Nothing in this test may ask the real Mac for anything. Asking is what
    /// makes macOS prompt, and a suite that prompts is a suite that puts a
    /// permission dialogue in front of whoever ran it.
    fn nothing_touches_the_real_mac() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| std::env::set_var("ERRAND_APPLE_DRY", "1"));
    }

    #[tokio::test]
    async fn the_screen_can_offer_an_enable_for_every_app_a_task_drives() {
        // The gap this closes: the consent button existed for the two channels
        // and for nothing else, so a task told to write a note met a prompt
        // nobody could see and simply stopped.
        nothing_touches_the_real_mac();
        let api = testkit::start().await;

        let listed = api.get("/v1/automation").await;
        let apps = listed["apps"].as_array().expect("a list of apps").clone();
        for app in ["notes", "mail_reading"] {
            let shown = apps.iter().find(|a| a["app"] == app).unwrap_or_else(|| {
                panic!("{app} is missing, so no screen can offer an Enable for it: {listed}")
            });
            assert!(
                shown["display_name"]
                    .as_str()
                    .unwrap_or_default()
                    .starts_with("Apple"),
                "the screen needs a name a person recognises: {shown}"
            );

            // Pressing Enable is the daemon asking macOS, and it answers with
            // the same shape, so the card redraws itself from the reply.
            let (code, answered) = api
                .post(&format!("/v1/automation/{app}/enable"), json!({}))
                .await;
            assert_eq!(code, 200, "{answered}");
            assert_eq!(answered["app"], app);
            assert!(answered["display_name"].is_string(), "{answered}");
        }

        // Reading the post is listed apart from sending it, and the screen says
        // how the two are related rather than quietly assuming one covers the
        // other.
        let note = listed["notes"]["mail_reading"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(note.contains("one macOS permission"), "{note}");
        assert!(note.contains("checks each on its own"), "{note}");

        let (code, body) = api
            .post("/v1/automation/the-garage-door/enable", json!({}))
            .await;
        assert_eq!(code, 400, "{body}");
    }

    #[tokio::test]
    async fn send_test_works_on_a_channel_once_you_have_said_where_you_are() {
        // The defect: the recipient was None for everything except Telegram, so
        // the button could never work and there was nowhere to set an address.
        let api = testkit::start().await;

        let (code, body) = api.post("/v1/channels/imessage/test", json!({})).await;
        assert_eq!(code, 409, "{body}");
        let detail = body["detail"].as_str().unwrap_or_default();
        assert!(
            detail.contains("Apple Messages"),
            "the refusal must name the channel: {detail}"
        );
        assert!(
            detail.contains("messaging.self.imessage"),
            "and say exactly which box to fill in: {detail}"
        );
        assert!(
            detail.contains("+1 555 0100"),
            "and what the right answer looks like: {detail}"
        );

        let (code, body) = api
            .post(
                "/v1/channels/imessage/config",
                json!({ "settings": { "messaging.self.imessage": " +1 555 0100 " } }),
            )
            .await;
        assert_eq!(code, 200, "{body}");
        assert_eq!(
            api.get("/v1/settings").await["messaging.self.imessage"],
            "+1 555 0100",
            "saved tidied, the way a recipient is"
        );

        let (code, body) = api.post("/v1/channels/imessage/test", json!({})).await;
        assert_eq!(code, 200, "the button must now work: {body}");

        // And the screen can say so, in words a person recognises.
        let listed = api.get("/v1/channels").await;
        let shown = listed["channels"]
            .as_array()
            .expect("a channel list")
            .iter()
            .find(|c| c["channel"] == "imessage")
            .expect("Apple Messages is one of them")
            .clone();
        assert_eq!(
            shown["display_name"], "Apple Messages",
            "the screen showed 'imessage', which is our word for it and nobody else's: {shown}"
        );
        assert_eq!(shown["self_address"], "+1 555 0100");

        // And the message that was queued is addressed to that person, not to
        // anything the caller sent.
        let queued = errand_core::db::due_outbox(&api.pool, 10)
            .await
            .expect("reading the outbox");
        let test = queued
            .iter()
            .find(|m| m.channel == "imessage")
            .expect("a queued test message");
        assert_eq!(test.recipient, "+1 555 0100");
    }

    #[tokio::test]
    async fn a_test_never_goes_anywhere_the_request_asks_it_to() {
        // A test that could name a recipient would be a way to make Errand send
        // anything to anyone, so the body is ignored entirely.
        let api = testkit::start().await;
        let (code, body) = api
            .post(
                "/v1/channels/apple_mail/config",
                json!({ "settings": { "messaging.self.apple_mail": "me@example.com" } }),
            )
            .await;
        assert_eq!(code, 200, "{body}");

        let (code, body) = api
            .post(
                "/v1/channels/apple_mail/test",
                json!({ "recipient": "stranger@example.com", "to": "stranger@example.com" }),
            )
            .await;
        assert_eq!(code, 200, "{body}");

        let queued = errand_core::db::due_outbox(&api.pool, 10)
            .await
            .expect("reading the outbox");
        let test = queued
            .iter()
            .find(|m| m.channel == "apple_mail")
            .expect("a queued test message");
        assert_eq!(test.recipient, "me@example.com");
    }

    #[tokio::test]
    async fn an_address_of_the_wrong_shape_is_refused_with_the_right_shape() {
        let api = testkit::start().await;
        for (channel, bad, expect) in [
            ("apple_mail", "0123456789", "someone@example.com"),
            ("imessage", "not an address", "+1 555 0100"),
            ("telegram", "+1 555 0100", "123456789"),
        ] {
            let key = format!("messaging.self.{channel}");
            let mut settings = serde_json::Map::new();
            settings.insert(key.clone(), json!(bad));
            let (code, body) = api
                .post(
                    &format!("/v1/channels/{channel}/config"),
                    json!({ "settings": settings }),
                )
                .await;
            assert_eq!(code, 400, "{channel}: {body}");
            let detail = body["detail"].as_str().unwrap_or_default();
            assert!(
                detail.contains(expect),
                "{channel} must say what the right shape looks like: {detail}"
            );
            assert!(
                errand_core::db::get_setting(&api.pool, &key)
                    .await
                    .expect("reading it back")
                    .is_none(),
                "a refused address must not be saved anyway"
            );
        }
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

    // ------------------------------------------- which models can do the task --

    /// A stand-in for a model server on somebody's network, answering the two
    /// things Errand asks of one: what it can run, and whether it will call a
    /// tool when it is given one.
    ///
    /// Raw TCP rather than a web framework, because the point is to prove
    /// Errand speaks the wire format, not to test a library. It counts the
    /// questions put to the model, which is how a test tells "it asked" from
    /// "it made somebody wait while it asked again for no reason".
    struct ModelServer {
        base: String,
        asked: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ModelServer {
        fn times_asked(&self) -> usize {
            self.asked.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    async fn a_model_server(answers_chat_with: &'static str) -> ModelServer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a free port");
        let port = listener.local_addr().expect("the port it took").port();
        let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = asked.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let counter = counter.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let asked = String::from_utf8_lossy(&buf[..n]).to_string();
                    let body = if asked.contains("/chat/completions") {
                        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        answers_chat_with.to_string()
                    } else {
                        json!({ "data": [{ "id": "qwen3.5-27b" }] }).to_string()
                    };
                    let res = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(res.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        ModelServer {
            base: format!("http://127.0.0.1:{port}"),
            asked,
        }
    }

    const CALLS_A_TOOL: &str = r#"{"choices":[{"message":{"role":"assistant","content":"",
        "tool_calls":[{"id":"c1","type":"function",
        "function":{"name":"report","arguments":"{\"answer\":\"yellow\"}"}}]}}]}"#;
    const ONLY_TALKS: &str =
        r#"{"choices":[{"message":{"role":"assistant","content":"Yellow."}}]}"#;

    async fn add_model(api: &testkit::Api, label: &str, base_url: &str) -> (String, Value) {
        let (code, body) = api
            .post(
                "/v1/ai/providers",
                json!({ "kind": "openai_compat", "label": label,
                        "base_url": base_url, "model": "qwen3.5-27b", "enabled": true }),
            )
            .await;
        assert_eq!(code, 200, "saving {label} failed: {body}");
        (
            body["id"].as_str().expect("an id").to_string(),
            body.clone(),
        )
    }

    fn listed<'a>(setup: &'a Value, id: &str) -> &'a Value {
        setup["providers"]
            .as_array()
            .expect("the list of models")
            .iter()
            .find(|p| p["id"] == id)
            .expect("the model that was just added")
    }

    #[tokio::test]
    async fn a_local_model_that_calls_a_tool_is_offered_the_job_of_doing_the_task() {
        // The complaint, answered. A model on somebody's own network that will
        // use a tool is capable of carrying out a task, and the screen has to
        // be able to say so rather than greying it out.
        let api = testkit::start().await;
        let server = a_model_server(CALLS_A_TOOL).await;
        let (id, saved) = add_model(&api, "The model on my desk", &server.base).await;

        assert!(
            saved["health_detail"]
                .as_str()
                .unwrap_or_default()
                .contains("can carry out tasks"),
            "saving it should say what it found out: {saved}"
        );

        let setup = api.get("/v1/ai").await;
        let p = listed(&setup, &id);
        assert_eq!(p["tools"], "yes");
        assert_eq!(
            p["cannot_carry_out_because"],
            Value::Null,
            "a model that used the tool must not be given a reason it cannot: {p}"
        );

        // And it can actually be chosen for the job, which is the part that was
        // refused before.
        let (code, body) = api
            .post("/v1/ai/roles/executor", json!({ "provider_id": id }))
            .await;
        assert_eq!(code, 200, "choosing it for the task failed: {body}");
        let setup = api.get("/v1/ai").await;
        let executor = &setup["roles"][0];
        assert_eq!(executor["role"], "executor");
        assert_eq!(executor["using"]["id"], json!(id));
        assert_eq!(executor["chosen_problem"], Value::Null);
    }

    #[tokio::test]
    async fn a_model_that_answers_in_words_is_refused_the_task_and_told_why() {
        let api = testkit::start().await;
        let server = a_model_server(ONLY_TALKS).await;
        let (id, _) = add_model(&api, "Small local model", &server.base).await;

        let setup = api.get("/v1/ai").await;
        let p = listed(&setup, &id);
        assert_eq!(p["tools"], "no");
        let why = p["cannot_carry_out_because"].as_str().unwrap_or_default();
        assert!(
            why.contains("did not use the tools"),
            "the reason has to be about this model: {why}"
        );
        assert!(
            why.contains("other three jobs"),
            "and it has to say what it can still do: {why}"
        );

        let (code, body) = api
            .post("/v1/ai/roles/executor", json!({ "provider_id": id }))
            .await;
        assert_eq!(code, 400, "it cannot do this job, so it cannot be picked");
        assert!(body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("did not use the tools"));

        // But it is still perfectly good at the rest.
        let (code, body) = api
            .post("/v1/ai/roles/narrator", json!({ "provider_id": id }))
            .await;
        assert_eq!(code, 200, "it can still write the messages: {body}");
    }

    #[tokio::test]
    async fn a_model_that_cannot_carry_out_a_task_cannot_be_named_by_one_either() {
        // The two ends have to agree. A model the AI screen refuses for the job
        // of doing the task must be refused on a task's own page as well, or
        // the way round the honest answer is to choose it one task at a time.
        let api = testkit::start().await;
        let server = a_model_server(ONLY_TALKS).await;
        let (model_id, _) = add_model(&api, "Small local model", &server.base).await;
        let task = a_task(&api, json!({ "name": "Court", "description": "Book it." })).await;

        let (code, body) = api
            .patch(
                &format!("/v1/tasks/{task}"),
                json!({ "model_id": model_id }),
            )
            .await;
        assert_eq!(code, 400, "it cannot do this job, so it cannot be named");
        assert!(
            body["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("did not use the tools"),
            "the refusal has to say what is wrong with this model: {body}"
        );
        assert_eq!(
            api.get(&format!("/v1/tasks/{task}")).await["model_id"],
            Value::Null,
            "a refused choice must not be half saved"
        );
    }

    #[tokio::test]
    async fn a_model_errand_could_not_reach_is_unchecked_rather_than_incapable() {
        // The line that made the app look like it was lying. Nothing was ever
        // asked of these models, so "cannot do this job" was never true.
        let api = testkit::start().await;
        // Port 9 discards everything, so nothing answers and nothing is learned.
        let (id, _) = add_model(&api, "The machine that is asleep", "http://127.0.0.1:9").await;

        let setup = api.get("/v1/ai").await;
        let p = listed(&setup, &id);
        assert_eq!(p["tools"], "unknown");
        assert!(
            p["tools_says"]
                .as_str()
                .unwrap_or_default()
                .contains("Not checked"),
            "an unasked model must read as unchecked: {p}"
        );
        assert_eq!(
            p["cannot_carry_out_because"],
            Value::Null,
            "not having checked is not a reason to call it incapable: {p}"
        );

        // And it can still be chosen: refusing it would be inventing a limit.
        let (code, body) = api
            .post("/v1/ai/roles/executor", json!({ "provider_id": id }))
            .await;
        assert_eq!(
            code, 200,
            "an unchecked model must still be pickable: {body}"
        );
    }

    #[tokio::test]
    async fn checking_again_updates_what_errand_knows_about_a_model() {
        let api = testkit::start().await;
        let server = a_model_server(CALLS_A_TOOL).await;
        let (id, _) = add_model(&api, "The mini PC", &server.base).await;

        let (code, body) = api
            .post(&format!("/v1/ai/providers/{id}/test"), json!({}))
            .await;
        assert_eq!(code, 200, "{body}");
        assert_eq!(body["tools"], "yes");
        assert!(body["tools_says"]
            .as_str()
            .unwrap_or_default()
            .contains("Can carry out tasks"));
        assert_eq!(
            server.times_asked(),
            2,
            "adding it asked once, and pressing Check asks again"
        );
    }

    #[tokio::test]
    async fn switching_a_model_off_does_not_put_it_through_the_questions_again() {
        // The switch on the row is the same save with one flag flipped. Asking
        // the model again there would leave somebody waiting on a machine they
        // are in the middle of turning off, and could learn nothing new.
        let api = testkit::start().await;
        let server = a_model_server(CALLS_A_TOOL).await;
        let (id, _) = add_model(&api, "The mini PC", &server.base).await;
        assert_eq!(server.times_asked(), 1, "adding it should ask once");

        let (code, body) = api
            .post(
                "/v1/ai/providers",
                json!({ "id": id, "kind": "openai_compat", "label": "The mini PC",
                        "base_url": server.base, "model": "qwen3.5-27b", "enabled": false }),
            )
            .await;
        assert_eq!(code, 200, "{body}");
        assert_eq!(
            server.times_asked(),
            1,
            "flipping a switch must not ask the model anything"
        );

        let setup = api.get("/v1/ai").await;
        assert_eq!(
            listed(&setup, &id)["tools"],
            "yes",
            "and what was learned about it must survive being switched off"
        );
    }

    #[tokio::test]
    async fn the_job_of_doing_the_task_no_longer_claims_it_has_to_be_claude() {
        // The card said this "must be Claude for now". It reached the screen
        // from here, so it is checked from here.
        let api = testkit::start().await;
        let setup = api.get("/v1/ai").await;
        let explains = setup["roles"][0]["explains"].as_str().unwrap_or_default();
        assert!(!explains.contains("Claude"), "{explains}");
        assert!(explains.contains("call tools"), "{explains}");
    }

    // ------------------------------------------------- which Claude, exactly --
    //
    // The complaint: the screen said "Claude (command line tool)" and stopped,
    // so somebody who picks between Opus, Sonnet and Haiku everywhere else had
    // no way to find out which one Errand was using, let alone change it.

    fn role_of<'a>(setup: &'a Value, name: &str) -> &'a Value {
        setup["roles"]
            .as_array()
            .expect("the four jobs")
            .iter()
            .find(|r| r["role"] == name)
            .unwrap_or_else(|| panic!("no job called {name}"))
    }

    /// The Claude row, added the same way a real install adds it.
    async fn with_claude(api: &testkit::Api) {
        crate::models::ensure_builtin(&api.pool)
            .await
            .expect("the built-in Claude row");
    }

    #[tokio::test]
    async fn the_ai_screen_says_which_claude_is_doing_each_job() {
        let api = testkit::start().await;
        with_claude(&api).await;
        let setup = api.get("/v1/ai").await;

        // Next to the provider, which is where the question gets asked.
        let claude = listed(&setup, crate::models::BUILTIN_CLAUDE);
        let summary = claude["models_in_use"].as_str().unwrap_or_default();
        assert!(
            summary.contains("Sonnet") && summary.contains("Haiku"),
            "the row has to name the models it is using: {claude}"
        );

        // And inside the job card, with what picking it means.
        let executor = role_of(&setup, "executor");
        assert_eq!(executor["using"]["model"], "sonnet");
        assert_eq!(executor["using"]["model_name"], "Sonnet");
        assert_eq!(executor["using"]["can_choose_model"], true);
        assert!(
            executor["using"]["model_says"]
                .as_str()
                .unwrap_or_default()
                .len()
                > 40,
            "a choice with no explanation is not a choice: {executor}"
        );
        assert_eq!(role_of(&setup, "narrator")["using"]["model"], "haiku");

        // The three on offer come from Errand, so the screen cannot offer one
        // the command line tool would refuse.
        let offered: Vec<&str> = setup["claude_models"]
            .as_array()
            .expect("the models on offer")
            .iter()
            .map(|m| m["alias"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(offered, ["opus", "sonnet", "haiku"]);
    }

    #[tokio::test]
    async fn choosing_opus_for_the_task_is_what_the_next_run_actually_asks_for() {
        let api = testkit::start().await;
        with_claude(&api).await;

        let (code, body) = api
            .post("/v1/ai/roles/executor/model", json!({ "model": "opus" }))
            .await;
        assert_eq!(code, 200, "choosing Opus failed: {body}");
        assert_eq!(body["model"], "opus");

        let setup = api.get("/v1/ai").await;
        let executor = role_of(&setup, "executor");
        assert_eq!(executor["using"]["model"], "opus");
        assert_eq!(executor["using"]["model_name"], "Opus");
        assert_eq!(
            role_of(&setup, "narrator")["using"]["model"],
            "haiku",
            "changing one job must not change the others"
        );
        assert!(
            listed(&setup, crate::models::BUILTIN_CLAUDE)["models_in_use"]
                .as_str()
                .unwrap_or_default()
                .contains("Opus"),
            "and the provider row has to agree with the job card"
        );

        // The part that makes it real: this is the exact call the executor
        // makes before it starts the agent, so what was chosen is what the
        // command line tool is asked for.
        let asked_for = errand_core::providers::claude_model_for(
            errand_core::providers::Role::Executor,
            &crate::models::claude_models(&api.state).await,
        );
        assert_eq!(asked_for, "opus");
    }

    #[tokio::test]
    async fn a_model_the_claude_tool_does_not_know_is_refused_with_the_ones_that_work() {
        // Saved and used later, this would fail the run at the hour the task
        // was meant to happen, with nobody watching.
        let api = testkit::start().await;
        with_claude(&api).await;

        let (code, body) = api
            .post("/v1/ai/roles/executor/model", json!({ "model": "gpt-4o" }))
            .await;
        assert_eq!(code, 400, "{body}");
        let why = body["detail"].as_str().unwrap_or_default();
        for name in ["Opus", "Sonnet", "Haiku"] {
            assert!(
                why.contains(name),
                "the refusal has to say what works: {why}"
            );
        }

        let setup = api.get("/v1/ai").await;
        assert_eq!(
            role_of(&setup, "executor")["using"]["model"],
            "sonnet",
            "a refused choice must not have been stored"
        );
    }

    #[tokio::test]
    async fn putting_a_job_back_to_no_choice_returns_it_to_the_model_it_started_on() {
        let api = testkit::start().await;
        with_claude(&api).await;

        api.post("/v1/ai/roles/fixer/model", json!({ "model": "opus" }))
            .await;
        assert_eq!(
            role_of(&api.get("/v1/ai").await, "fixer")["using"]["model"],
            "opus"
        );

        let (code, body) = api
            .post("/v1/ai/roles/fixer/model", json!({ "model": Value::Null }))
            .await;
        assert_eq!(code, 200, "{body}");
        assert_eq!(body["model"], "haiku", "back to what Errand would pick");
        assert_eq!(
            role_of(&api.get("/v1/ai").await, "fixer")["using"]["model"],
            "haiku"
        );
    }

    #[tokio::test]
    async fn a_model_of_your_own_is_not_offered_a_claude_to_choose_from() {
        // The standing rule for this screen: never show a control that would
        // change nothing. A model on your desk has one model, chosen when it
        // was added.
        let api = testkit::start().await;
        let server = a_model_server(CALLS_A_TOOL).await;
        let (id, _) = add_model(&api, "The model on my desk", &server.base).await;
        api.post("/v1/ai/roles/narrator", json!({ "provider_id": id }))
            .await;

        let setup = api.get("/v1/ai").await;
        let narrator = role_of(&setup, "narrator");
        assert_eq!(narrator["using"]["id"], json!(id));
        assert_eq!(narrator["using"]["can_choose_model"], false);
        assert_eq!(narrator["using"]["model"], "qwen3.5-27b");
        assert_eq!(narrator["using"]["model_name"], Value::Null);
        assert_eq!(
            listed(&setup, &id)["models_in_use"],
            Value::Null,
            "only the command line tool has a model per job"
        );
    }
}

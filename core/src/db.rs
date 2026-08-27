//! Database access. The daemon owns this file exclusively.
//!
//! The UI never opens the database, not even read-only: a read-only connection
//! to a WAL database needs the shared-memory file and effectively a read-write
//! peer, so it fails exactly when the daemon is down and you are trying to work
//! out why. The UI goes through the API for everything.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use std::str::FromStr;

pub type Pool = SqlitePool;

/// Open the database, running any pending migrations. Migrations run here, in
/// the daemon, and nowhere else.
pub async fn open() -> Result<Pool> {
    let path = crate::paths::db_path()?;
    crate::paths::ensure_dirs()?;

    let url = format!("sqlite://{}", path.to_string_lossy());
    let opts = SqliteConnectOptions::from_str(&url)?
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(5));

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await
        .with_context(|| format!("opening {}", path.display()))?;

    migrate(&pool).await?;
    Ok(pool)
}

/// Open an in-memory database with the schema applied. Used by tests.
pub async fn open_memory() -> Result<Pool> {
    let opts = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await?;
    migrate(&pool).await?;
    Ok(pool)
}

async fn migrate(pool: &Pool) -> Result<()> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .context("running migrations")?;
    Ok(())
}

/// `PRAGMA quick_check`, run at boot before anything trusts the file.
pub async fn quick_check(pool: &Pool) -> Result<String> {
    let row = sqlx::query("PRAGMA quick_check").fetch_one(pool).await?;
    Ok(row.try_get::<String, _>(0).unwrap_or_else(|_| "ok".into()))
}

// ---------------------------------------------------------------- settings --

pub async fn get_setting(pool: &Pool, key: &str) -> Result<Option<serde_json::Value>> {
    let row = sqlx::query("SELECT value_json FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    match row {
        Some(r) => {
            let raw: String = r.try_get("value_json")?;
            Ok(Some(serde_json::from_str(&raw)?))
        }
        None => Ok(None),
    }
}

pub async fn set_setting(pool: &Pool, key: &str, value: &serde_json::Value) -> Result<()> {
    sqlx::query(
        "INSERT INTO settings (key, value_json, updated_at) VALUES (?, ?, ?)
         ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json,
                                        updated_at = excluded.updated_at",
    )
    .bind(key)
    .bind(serde_json::to_string(value)?)
    .bind(crate::now_iso())
    .execute(pool)
    .await?;
    Ok(())
}

// ------------------------------------------------------------------- tasks --

pub async fn list_tasks(pool: &Pool, include_archived: bool) -> Result<Vec<crate::models::Task>> {
    let sql = if include_archived {
        "SELECT * FROM tasks ORDER BY created_at DESC"
    } else {
        "SELECT * FROM tasks WHERE status <> 'archived' ORDER BY created_at DESC"
    };
    let rows = sqlx::query(sql).fetch_all(pool).await?;
    rows.iter().map(task_from_row).collect()
}

pub async fn get_task(pool: &Pool, id: &str) -> Result<Option<crate::models::Task>> {
    let row = sqlx::query("SELECT * FROM tasks WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(task_from_row).transpose()
}

fn task_from_row(r: &sqlx::sqlite::SqliteRow) -> Result<crate::models::Task> {
    let parse = |s: String| -> serde_json::Value {
        serde_json::from_str(&s).unwrap_or(serde_json::Value::Null)
    };
    Ok(crate::models::Task {
        id: r.try_get("id")?,
        name: r.try_get("name")?,
        emoji: r.try_get("emoji")?,
        description: r.try_get("description_md")?,
        understanding: r.try_get("understanding_md")?,
        status: r.try_get("status")?,
        schedule: parse(r.try_get("schedule_json")?),
        notify: parse(r.try_get("notify_json")?),
        limits: parse(r.try_get("limits_json")?),
        allowed_domains: parse(r.try_get("allowed_domains_json")?),
        model_id: crate::providers::read_task_model(
            r.try_get::<Option<String>, _>("model_roles_json")?
                .as_deref(),
        ),
        playbook_version: r.try_get("active_playbook_version")?,
        next_run_at: r.try_get("next_run_at")?,
        paused_reason: r.try_get("paused_reason")?,
        auto_paused: r.try_get::<i64, _>("auto_paused")? != 0,
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
    })
}

/// A password belongs in the keychain, and a description is the one field that
/// is sent whole to a model on every run.
///
/// Checked here as well as at the API edge, for the reason the sites are: this
/// is the only place every write passes through, so it is the only place the
/// rule cannot be walked around by a caller that forgot it. See
/// `crate::passwords` for what counts and why the shape is drawn where it is.
fn refuse_a_typed_secret(description: &str) -> Result<()> {
    if let Some(label) = crate::passwords::typed_secret(description) {
        anyhow::bail!(crate::passwords::refusal(&label));
    }
    Ok(())
}

pub struct NewTask {
    pub name: String,
    pub description: String,
    pub emoji: Option<String>,
    pub schedule: serde_json::Value,
}

pub async fn create_task(pool: &Pool, t: NewTask) -> Result<crate::models::Task> {
    refuse_a_typed_secret(&t.description)?;
    let id = crate::new_id();
    let now = crate::now_iso();
    // The floor starts at creation. A task set up today with a cron that has
    // been notionally due every morning for the past year has no missed runs to
    // make up: it did not exist for any of them. Without this, its first tick
    // would replay history.
    sqlx::query(
        "INSERT INTO tasks (id, name, emoji, description_md, status, schedule_json,
                            catch_up_floor_at, created_at, updated_at)
         VALUES (?, ?, ?, ?, 'draft', ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&t.name)
    .bind(&t.emoji)
    .bind(&t.description)
    .bind(serde_json::to_string(&t.schedule)?)
    .bind(&now)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    get_task(pool, &id)
        .await?
        .context("task vanished immediately after insert")
}

/// What a person may change about a task once it exists.
///
/// Every field is optional and absent means unchanged, so a screen that only
/// edits the schedule cannot blank the description on the way past.
#[derive(Debug, Clone, Default)]
pub struct TaskPatch {
    pub name: Option<String>,
    pub emoji: Option<String>,
    pub description: Option<String>,
    /// Replaces the stored schedule outright, unlike the two below. That is
    /// deliberate: a schedule is one shape rather than a bag of settings, and
    /// wholesale replacement is what lets a run window be taken off a task
    /// again. Merging here would make a window impossible to remove.
    pub schedule: Option<serde_json::Value>,
    /// Merged over what is stored, key by key.
    pub notify: Option<serde_json::Value>,
    /// Merged over what is stored, key by key, so raising one ceiling cannot
    /// silently reset the four the caller did not mention.
    pub limits: Option<serde_json::Value>,
    /// Sites as typed. Normalised here rather than at the edge, so there is no
    /// route by which an entry the run-time check could never match gets
    /// written to the database.
    pub allowed_domains: Option<Vec<String>>,
    /// Which model carries this task out. Its own three-valued type rather than
    /// an `Option`, because here the difference between "not mentioned" and
    /// "put it back on the default" is the difference between a task keeping
    /// the model it was given and quietly losing it when somebody edits its
    /// sites.
    pub model: crate::providers::ModelChoice,
}

/// Lay a patch over a stored settings object, one key at a time.
///
/// `None` means the caller did not mention this setting at all, so the column
/// is left alone. An object is merged rather than substituted, because the
/// callers upstream send only what changed: a body naming `max_usd` alone must
/// not take `max_messages` with it. Anything that is not an object (a null, a
/// list, a stored value that is not readable as JSON) replaces what is there,
/// since there is nothing to merge into.
fn merge_settings(stored: &str, patch: Option<&serde_json::Value>) -> Result<Option<String>> {
    let Some(patch) = patch else {
        return Ok(None);
    };
    let merged = match (
        serde_json::from_str::<serde_json::Value>(stored),
        patch.as_object(),
    ) {
        (Ok(serde_json::Value::Object(mut base)), Some(over)) => {
            for (key, value) in over {
                base.insert(key.clone(), value.clone());
            }
            serde_json::Value::Object(base)
        }
        _ => patch.clone(),
    };
    Ok(Some(serde_json::to_string(&merged)?))
}

/// Change a task's settings, in one transaction.
///
/// Status, auto_paused, paused_reason, existing runs and existing side effects
/// are all left alone, deliberately. Editing a schedule must never un-pause a
/// task that was paused because an irreversible action needs checking: the
/// person editing the schedule has not necessarily looked at the site.
pub async fn update_task(pool: &Pool, id: &str, patch: TaskPatch) -> Result<crate::models::Task> {
    let now = crate::now_iso();

    if let Some(d) = &patch.description {
        refuse_a_typed_secret(d)?;
    }

    // Done before the transaction opens, so a rejected site entry costs nothing
    // and leaves nothing half-written.
    let domains_json = match &patch.allowed_domains {
        Some(list) => Some(serde_json::to_string(
            &crate::domains::normalize_domains(list)?.domains,
        )?),
        None => None,
    };
    let schedule_json = patch
        .schedule
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;

    // Also read before the transaction opens. A transaction takes a connection
    // of its own, so reading back through the pool while one is open would have
    // the caller waiting on itself whenever the pool is small.
    let previous_floor = task_catch_up_floor(pool, id).await?;

    let mut tx = pool.begin().await?;

    let row = sqlx::query(
        "SELECT status, schedule_json, notify_json, limits_json, model_roles_json
         FROM tasks WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        anyhow::bail!("There is no task with id {id}, so nothing was changed.");
    };
    let status: String = row.try_get("status")?;
    if status == "archived" {
        anyhow::bail!(
            "This task has been archived, so its settings can no longer be changed. Nothing was \
             altered. Restore it first if you want to edit it."
        );
    }
    let current_schedule: String = row.try_get("schedule_json")?;

    // Limits and notify preferences are merged key by key over what is stored,
    // read inside the transaction so nothing can change underneath the merge.
    // A screen that edits one number sends one number, and replacing the whole
    // object with it would quietly drop the other four: a task whose owner set
    // a ceiling of one message would fall back to the default of three because
    // somebody adjusted its spending limit. A schedule is deliberately not
    // treated this way; see the field's own note.
    let notify_json = merge_settings(
        &row.try_get::<String, _>("notify_json")?,
        patch.notify.as_ref(),
    )?;
    let limits_json = merge_settings(
        &row.try_get::<String, _>("limits_json")?,
        patch.limits.as_ref(),
    )?;

    // Which model carries the task out. Read inside the transaction like the
    // two above, and rewritten rather than replaced, so naming a model cannot
    // take the rest of that column with it. An edit that says nothing about the
    // model leaves the column untouched: a task told to use the machine under
    // the desk must still be using it after somebody edits its sites.
    let model_changing = patch.model != crate::providers::ModelChoice::Unchanged;
    let model_roles_json = {
        let stored: Option<String> = row.try_get("model_roles_json")?;
        match &patch.model {
            crate::providers::ModelChoice::Unchanged => None,
            crate::providers::ModelChoice::Default => {
                crate::providers::write_task_model(stored.as_deref(), None)
            }
            crate::providers::ModelChoice::Named(model_id) => {
                crate::providers::write_task_model(stored.as_deref(), Some(model_id))
            }
        }
    };

    let mut floor_at: Option<String> = None;
    let mut next_run_at: Option<String> = None;
    let mut schedule_changing = false;

    if let Some(new_value) = &patch.schedule {
        let spec = crate::schedule::ScheduleSpec::from_json(new_value)?;
        spec.validate()?;

        // Compared through the spec rather than as raw text: the same schedule
        // written out with its defaults spelled in is still the same schedule,
        // and treating that as a change would move the floor for nothing.
        let current: serde_json::Value =
            serde_json::from_str(&current_schedule).unwrap_or(serde_json::Value::Null);
        schedule_changing = crate::schedule::ScheduleSpec::from_json(&current)
            .map(|c| c.to_json() != spec.to_json())
            .unwrap_or(true);

        if schedule_changing {
            // The last scheduled occurrence this task has already recorded.
            //
            // The LIKE is load-bearing. Manual occurrence ids look like
            // "manual/<uuid>", and 'm' sorts above '2', so a bare MAX() would
            // hand back a manual id and the floor would be nonsense. Scheduled
            // ids and now_iso() share the %Y-%m-%dT%H:%M:%SZ format, which is
            // why they can be compared as text and still mean instants.
            let last: Option<String> = sqlx::query(
                "SELECT MAX(occurrence_id) AS last FROM runs
                 WHERE task_id = ? AND occurrence_id LIKE '____-__-__T__:__:__Z'",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?
            .try_get("last")?;

            // The floor only ever moves forward. An earlier edit already closed
            // those occurrences off, and letting a later edit re-open them
            // would be the very replay this is here to stop.
            let mut floor = now.clone();
            for candidate in [last.as_deref(), previous_floor.as_deref()]
                .into_iter()
                .flatten()
            {
                if candidate > floor.as_str() {
                    floor = candidate.to_string();
                }
            }

            let floor_dt: DateTime<Utc> = floor
                .parse()
                .with_context(|| format!("reading the catch-up floor '{floor}'"))?;

            // Counted from the floor, not from now, or the countdown would
            // promise a run that the floor is about to refuse. Bound to NULL
            // for a manual schedule or a one-shot whose moment has gone,
            // because nothing else in the system ever clears this column and a
            // stale "next run" would sit there forever.
            next_run_at = spec.next_after(floor_dt)?.map(|occurrence| {
                // The expression the scheduler uses for the same number, so the
                // time shown is the time it happens and does not jump every
                // twenty seconds.
                (spec.start_at(occurrence)
                    + crate::schedule::jitter_for(id, occurrence, spec.jitter_s))
                .to_rfc3339()
            });
            floor_at = Some(floor);
        }
    }

    sqlx::query(
        "UPDATE tasks SET
            name                 = COALESCE(?, name),
            emoji                = COALESCE(?, emoji),
            description_md       = COALESCE(?, description_md),
            schedule_json        = COALESCE(?, schedule_json),
            notify_json          = COALESCE(?, notify_json),
            limits_json          = COALESCE(?, limits_json),
            allowed_domains_json = COALESCE(?, allowed_domains_json),
            -- Not COALESCE, because putting a task back on the default means
            -- writing NULL here, and COALESCE cannot tell that apart from an
            -- edit that never mentioned the model.
            model_roles_json     = CASE WHEN ? = 1 THEN ? ELSE model_roles_json END,
            catch_up_floor_at    = CASE WHEN ? = 1 THEN ? ELSE catch_up_floor_at END,
            next_run_at          = CASE WHEN ? = 1 THEN ? ELSE next_run_at END,
            updated_at           = ?
         WHERE id = ?",
    )
    .bind(&patch.name)
    .bind(&patch.emoji)
    .bind(&patch.description)
    .bind(&schedule_json)
    .bind(&notify_json)
    .bind(&limits_json)
    .bind(&domains_json)
    .bind(i64::from(model_changing))
    .bind(&model_roles_json)
    .bind(i64::from(schedule_changing))
    .bind(&floor_at)
    .bind(i64::from(schedule_changing))
    .bind(&next_run_at)
    .bind(&now)
    .bind(id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    get_task(pool, id)
        .await?
        .context("task vanished immediately after being changed")
}

/// The instant below which this task's schedule did not yet exist.
///
/// The scheduler must not enqueue any occurrence earlier than this. Its
/// catch-up cursor is global, so without the floor a task whose schedule
/// changed a minute ago looks like a task that has been missing runs all week.
/// `None` means no floor: a task that has been running against its current
/// schedule all along.
pub async fn task_catch_up_floor(pool: &Pool, task_id: &str) -> Result<Option<String>> {
    let row = sqlx::query("SELECT catch_up_floor_at FROM tasks WHERE id = ?")
        .bind(task_id)
        .fetch_optional(pool)
        .await?;
    Ok(row
        .map(|r| r.try_get::<Option<String>, _>("catch_up_floor_at"))
        .transpose()?
        .flatten())
}

/// Pause keeps the computed next run visible but the scheduler never enqueues
/// it, and unpausing does not replay what was missed.
pub async fn set_task_paused(
    pool: &Pool,
    id: &str,
    paused: bool,
    reason: Option<&str>,
) -> Result<bool> {
    let status = if paused { "paused" } else { "ready" };
    let res = sqlx::query(
        "UPDATE tasks SET status = ?, paused_reason = ?, auto_paused = 0, updated_at = ?
         WHERE id = ? AND status IN ('ready','paused')",
    )
    .bind(status)
    .bind(reason)
    .bind(crate::now_iso())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

// -------------------------------------------------------------------- runs --

pub async fn list_runs(
    pool: &Pool,
    task_id: Option<&str>,
    limit: i64,
) -> Result<Vec<crate::models::Run>> {
    let rows =
        match task_id {
            Some(t) => sqlx::query(
                "SELECT * FROM runs WHERE task_id = ? ORDER BY created_at DESC, id DESC LIMIT ?",
            )
            .bind(t)
            .bind(limit)
            .fetch_all(pool)
            .await?,
            None => {
                sqlx::query("SELECT * FROM runs ORDER BY created_at DESC, id DESC LIMIT ?")
                    .bind(limit)
                    .fetch_all(pool)
                    .await?
            }
        };
    rows.iter().map(run_from_row).collect()
}

pub async fn get_run(pool: &Pool, id: &str) -> Result<Option<crate::models::Run>> {
    let row = sqlx::query("SELECT * FROM runs WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(run_from_row).transpose()
}

/// The newest run of every task, in one query.
///
/// The task list needs this: a task can say "Learning" while the run it is
/// learning from finished hours ago, and a screen that shows the stored word
/// without the run behind it is telling somebody something untrue. One query
/// rather than one per task, because the list is drawn on every visit.
///
/// Newest by id, not by created_at: ids are time-ordered, and two runs created
/// in the same second would otherwise both win.
pub async fn latest_run_per_task(
    pool: &Pool,
) -> Result<std::collections::HashMap<String, crate::models::Run>> {
    let rows = sqlx::query(
        "SELECT r.* FROM runs r
           JOIN (SELECT task_id, MAX(id) AS newest FROM runs GROUP BY task_id) x
             ON x.task_id = r.task_id AND x.newest = r.id",
    )
    .fetch_all(pool)
    .await?;

    let mut out = std::collections::HashMap::new();
    for row in &rows {
        let run = run_from_row(row)?;
        out.insert(run.task_id.clone(), run);
    }
    Ok(out)
}

fn run_from_row(r: &sqlx::sqlite::SqliteRow) -> Result<crate::models::Run> {
    let code: Option<String> = r.try_get("failure_code")?;
    let human: Option<String> = r.try_get("failure_human")?;
    let failure = match (code, human) {
        (Some(c), Some(h)) => Some(crate::models::Failure {
            code: c,
            plain_reason: h,
            fix: r.try_get("failure_fix")?,
            technical: r.try_get("failure_technical")?,
        }),
        _ => None,
    };
    Ok(crate::models::Run {
        id: r.try_get("id")?,
        task_id: r.try_get("task_id")?,
        occurrence_id: r.try_get("occurrence_id")?,
        mode: r.try_get("mode")?,
        // The mode is read as well as the flag, and once, here. A run recorded
        // as a rehearsal before the flag existed says so in its mode alone, and
        // this is the only place that has to know it.
        rehearsal: r.try_get::<i64, _>("rehearsal")? != 0
            || r.try_get::<String, _>("mode")? == crate::models::RunMode::REHEARSAL.stored(),
        trigger: r.try_get("trigger")?,
        triggered_by: r.try_get("triggered_by")?,
        status: r.try_get("status")?,
        scheduled_for: r.try_get("scheduled_for")?,
        started_at: r.try_get("started_at")?,
        finished_at: r.try_get("finished_at")?,
        summary: r.try_get("summary_md")?,
        answer: r.try_get("answer_md")?,
        failure,
        tokens_in: r.try_get("tokens_in")?,
        tokens_out: r.try_get("tokens_out")?,
        cost_usd: r.try_get("cost_usd")?,
        created_at: r.try_get("created_at")?,
    })
}

/// Why creating a run did not happen.
#[derive(Debug)]
pub enum CreateRunError {
    /// This occurrence already produced a run. Expected, and not a problem.
    AlreadyExists,
    /// Anything else. Must never be mistaken for the above: treating a disk
    /// error or a busy database as "already ran" silently loses the occurrence
    /// forever, with no run, no failure and no explanation.
    Other(anyhow::Error),
}

impl std::fmt::Display for CreateRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyExists => write!(f, "this occurrence already has a run"),
            Self::Other(e) => write!(f, "{e}"),
        }
    }
}

/// Create a run for one occurrence, distinguishing a duplicate from a fault.
pub async fn try_create_run(
    pool: &Pool,
    task_id: &str,
    occurrence_id: &str,
    trigger: &str,
    mode: crate::models::RunMode,
    triggered_by: Option<&str>,
) -> std::result::Result<crate::models::Run, CreateRunError> {
    let id = crate::new_id();
    let res = sqlx::query(
        "INSERT INTO runs (id, task_id, occurrence_id, mode, rehearsal, trigger, triggered_by,
                           status, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, 'queued', ?)",
    )
    .bind(&id)
    .bind(task_id)
    .bind(occurrence_id)
    .bind(mode.stored())
    .bind(i64::from(mode.is_rehearsal()))
    .bind(trigger)
    .bind(triggered_by)
    .bind(crate::now_iso())
    .execute(pool)
    .await;

    if let Err(e) = res {
        // Only a unique-index violation means "already ran".
        let is_dup = match &e {
            sqlx::Error::Database(db) => {
                db.code().as_deref() == Some("2067")
                    || db.message().contains("runs.task_id, runs.occurrence_id")
                    || db.message().contains("idx_runs_occurrence")
            }
            _ => false,
        };
        return Err(if is_dup {
            CreateRunError::AlreadyExists
        } else {
            CreateRunError::Other(e.into())
        });
    }

    get_run(pool, &id)
        .await
        .map_err(CreateRunError::Other)?
        .ok_or_else(|| CreateRunError::Other(anyhow::anyhow!("run vanished after insert")))
}

/// Create a run for one occurrence. The unique index on
/// `(task_id, occurrence_id)` makes a duplicate insert fail rather than
/// producing a second run for the same scheduled slot.
pub async fn create_run(
    pool: &Pool,
    task_id: &str,
    occurrence_id: &str,
    trigger: &str,
    mode: crate::models::RunMode,
    triggered_by: Option<&str>,
) -> Result<crate::models::Run> {
    let id = crate::new_id();
    sqlx::query(
        "INSERT INTO runs (id, task_id, occurrence_id, mode, rehearsal, trigger, triggered_by,
                           status, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, 'queued', ?)",
    )
    .bind(&id)
    .bind(task_id)
    .bind(occurrence_id)
    .bind(mode.stored())
    .bind(i64::from(mode.is_rehearsal()))
    .bind(trigger)
    .bind(triggered_by)
    .bind(crate::now_iso())
    .execute(pool)
    .await?;
    get_run(pool, &id)
        .await?
        .context("run vanished after insert")
}

pub async fn count_busy_runs(pool: &Pool) -> Result<i64> {
    let row = sqlx::query(
        "SELECT COUNT(*) AS n FROM runs
         WHERE status IN ('armed','queued','preflight','holding','running','healing',
                          'waiting_input','takeover')",
    )
    .fetch_one(pool)
    .await?;
    Ok(row.try_get("n")?)
}

pub async fn list_steps(pool: &Pool, run_id: &str) -> Result<Vec<crate::models::Step>> {
    let rows = sqlx::query("SELECT * FROM run_steps WHERE run_id = ? ORDER BY seq")
        .bind(run_id)
        .fetch_all(pool)
        .await?;
    rows.into_iter()
        .map(|r| {
            let detail: Option<String> = r.try_get("detail_json")?;
            Ok(crate::models::Step {
                seq: r.try_get("seq")?,
                ts: r.try_get("ts")?,
                kind: r.try_get("kind")?,
                title: r.try_get("title")?,
                ok: r.try_get::<i64, _>("ok")? != 0,
                detail: detail.and_then(|d| serde_json::from_str(&d).ok()),
                artifact_id: r.try_get("artifact_id")?,
                duration_ms: r.try_get("duration_ms")?,
            })
        })
        .collect()
}

/// Record a file a run left behind. The id is minted here and the row is
/// addressed by it alone: a request can name an artifact id, never a path, so
/// being asked for a file can never be turned into reading an arbitrary one.
pub async fn record_artifact(
    pool: &Pool,
    run_id: &str,
    kind: &str,
    rel_path: &str,
    bytes: i64,
) -> Result<String> {
    let id = crate::new_id();
    sqlx::query(
        "INSERT INTO run_artifacts (id, run_id, kind, rel_path, masked, bytes)
         VALUES (?, ?, ?, ?, 1, ?)",
    )
    .bind(&id)
    .bind(run_id)
    .bind(kind)
    .bind(rel_path)
    .bind(bytes)
    .execute(pool)
    .await?;
    Ok(id)
}

/// Point a journal step at the artifact it produced.
pub async fn attach_step_artifact(
    pool: &Pool,
    run_id: &str,
    seq: i64,
    artifact_id: &str,
) -> Result<()> {
    sqlx::query("UPDATE run_steps SET artifact_id = ? WHERE run_id = ? AND seq = ?")
        .bind(artifact_id)
        .bind(run_id)
        .bind(seq)
        .execute(pool)
        .await?;
    Ok(())
}

/// Look an artifact up by id.
pub async fn get_artifact(pool: &Pool, id: &str) -> Result<Option<crate::models::Artifact>> {
    let row = sqlx::query("SELECT * FROM run_artifacts WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.map(|r| {
        Ok(crate::models::Artifact {
            id: r.try_get("id")?,
            run_id: r.try_get("run_id")?,
            kind: r.try_get("kind")?,
            rel_path: r.try_get("rel_path")?,
            masked: r.try_get::<i64, _>("masked")? != 0,
            bytes: r.try_get("bytes")?,
            created_at: r.try_get("created_at")?,
        })
    })
    .transpose()
}

/// Append one journal step. Journal-then-act is the rule: a step is recorded
/// before the action it describes proceeds, so a crash leaves evidence of the
/// last thing attempted.
pub async fn append_step(
    pool: &Pool,
    run_id: &str,
    kind: &str,
    title: &str,
    ok: bool,
    detail: Option<&serde_json::Value>,
) -> Result<i64> {
    let row =
        sqlx::query("SELECT COALESCE(MAX(seq), 0) + 1 AS next FROM run_steps WHERE run_id = ?")
            .bind(run_id)
            .fetch_one(pool)
            .await?;
    let seq: i64 = row.try_get("next")?;
    sqlx::query(
        "INSERT INTO run_steps (run_id, seq, ts, kind, title, detail_json, ok)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(run_id)
    .bind(seq)
    .bind(crate::now_iso())
    .bind(kind)
    .bind(title)
    .bind(detail.map(|d| d.to_string()))
    .bind(if ok { 1 } else { 0 })
    .execute(pool)
    .await?;
    Ok(seq)
}

pub async fn set_run_status(pool: &Pool, run_id: &str, status: &str) -> Result<()> {
    let started = if status == "running" {
        Some(crate::now_iso())
    } else {
        None
    };
    sqlx::query("UPDATE runs SET status = ?, started_at = COALESCE(started_at, ?) WHERE id = ?")
        .bind(status)
        .bind(started)
        .bind(run_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Close a run as done, with the story of the work and the answer it produced.
///
/// The two are separate columns because they are separate things, and the app
/// had only the first of them for too long: a person opening a finished task
/// was shown what it did and never what it found.
pub async fn finish_run_ok(
    pool: &Pool,
    run_id: &str,
    summary: &str,
    answer: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE runs SET status = 'succeeded', finished_at = ?, summary_md = ?, answer_md = ?
         WHERE id = ?",
    )
    .bind(crate::now_iso())
    .bind(summary)
    .bind(answer)
    .bind(run_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Close a run as failed. The schema rejects this without both a code and a
/// plain-language explanation, which is the invariant that keeps a failure from
/// ever reaching the user as a bare error string.
pub async fn finish_run_failed(
    pool: &Pool,
    run_id: &str,
    code: &str,
    human: &str,
    technical: Option<&str>,
) -> Result<()> {
    finish_run_failed_with_answer(pool, run_id, code, human, technical, None).await
}

/// The same, for a run that failed holding an answer anyway.
///
/// That combination is not a curiosity, it is the common case: a run reads the
/// mail, works out the summary, and only then finds that macOS will not let it
/// write the note it was told to write. The run is genuinely a failure, and
/// throwing away what it found would make a person do the work again by hand.
pub async fn finish_run_failed_with_answer(
    pool: &Pool,
    run_id: &str,
    code: &str,
    human: &str,
    technical: Option<&str>,
    answer: Option<&str>,
) -> Result<()> {
    finish_run_failed_fully(pool, run_id, code, human, None, technical, answer).await
}

/// Close a run as failed, with everything a person and a screen need.
///
/// `human` is one line about what stopped it and `fix` one line about what to
/// do. They are separate because the screen shows them differently, and because
/// a failure with nothing to be done about it should say nothing rather than
/// pad.
pub async fn finish_run_failed_fully(
    pool: &Pool,
    run_id: &str,
    code: &str,
    human: &str,
    fix: Option<&str>,
    technical: Option<&str>,
    answer: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE runs SET status = 'failed', finished_at = ?, failure_code = ?,
                         failure_human = ?, failure_fix = ?, failure_technical = ?,
                         answer_md = COALESCE(?, answer_md)
         WHERE id = ?",
    )
    .bind(crate::now_iso())
    .bind(code)
    .bind(human)
    .bind(fix)
    .bind(technical)
    .bind(answer)
    .bind(run_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Where a run put a copy of its answer, and what it is called.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AnswerCopy {
    pub id: String,
    pub kind: String,
    pub label: String,
    /// Never sent to a browser. Opening one goes through the delivery's id, so
    /// a caller cannot name a file of its own choosing and have it opened.
    #[serde(skip_serializing)]
    pub locator: String,
}

/// Write down that a run left a copy of its answer somewhere.
///
/// Called by the tool that did it, at the point it succeeded, so a link on
/// screen always corresponds to something that really happened.
pub async fn record_answer_copy(
    pool: &Pool,
    run_id: &str,
    kind: &str,
    label: &str,
    locator: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO run_answer_copies (id, run_id, kind, label, locator, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(crate::new_id())
    .bind(run_id)
    .bind(kind)
    .bind(label)
    .bind(locator)
    .bind(crate::now_iso())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_answer_copies(pool: &Pool, run_id: &str) -> Result<Vec<AnswerCopy>> {
    let rows = sqlx::query(
        "SELECT id, kind, label, locator FROM run_answer_copies
         WHERE run_id = ? ORDER BY created_at, rowid",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(AnswerCopy {
                id: r.try_get("id")?,
                kind: r.try_get("kind")?,
                label: r.try_get("label")?,
                locator: r.try_get("locator")?,
            })
        })
        .collect()
}

/// One copy, by its own id, for opening it.
pub async fn get_answer_copy(pool: &Pool, id: &str) -> Result<Option<AnswerCopy>> {
    let row = sqlx::query("SELECT id, kind, label, locator FROM run_answer_copies WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    match row {
        None => Ok(None),
        Some(r) => Ok(Some(AnswerCopy {
            id: r.try_get("id")?,
            kind: r.try_get("kind")?,
            label: r.try_get("label")?,
            locator: r.try_get("locator")?,
        })),
    }
}

pub async fn record_usage(
    pool: &Pool,
    run_id: &str,
    tokens_in: i64,
    tokens_out: i64,
    cost_usd: f64,
) -> Result<()> {
    sqlx::query(
        "UPDATE runs SET tokens_in = tokens_in + ?, tokens_out = tokens_out + ?,
                         cost_usd = cost_usd + ?
         WHERE id = ?",
    )
    .bind(tokens_in)
    .bind(tokens_out)
    .bind(cost_usd)
    .bind(run_id)
    .execute(pool)
    .await?;
    Ok(())
}

// ------------------------------------------------------------- credentials --

pub async fn list_credentials(pool: &Pool) -> Result<Vec<crate::models::CredentialMeta>> {
    let rows = sqlx::query("SELECT * FROM credentials ORDER BY label")
        .fetch_all(pool)
        .await?;
    rows.into_iter()
        .map(|r| {
            Ok(crate::models::CredentialMeta {
                id: r.try_get("id")?,
                label: r.try_get("label")?,
                kind: r.try_get("kind")?,
                domain: r.try_get("domain")?,
                username: r.try_get("username")?,
                require_biometric: r.try_get::<i64, _>("require_biometric")? != 0,
                last_used_at: r.try_get("last_used_at")?,
                use_count: r.try_get("use_count")?,
                created_at: r.try_get("created_at")?,
            })
        })
        .collect()
}

/// Record the metadata for a credential. The secret itself goes to the keychain
/// separately and never passes through this function.
pub async fn create_credential_meta(
    pool: &Pool,
    label: &str,
    kind: &str,
    domain: &str,
    username: Option<&str>,
) -> Result<String> {
    let id = crate::new_id();
    sqlx::query(
        "INSERT INTO credentials (id, label, kind, username, domain,
                                  keychain_service, keychain_account, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(label)
    .bind(kind)
    .bind(username)
    .bind(domain)
    .bind(crate::keychain_service())
    .bind(format!("cred/{id}/v1"))
    .bind(crate::now_iso())
    .execute(pool)
    .await?;
    Ok(id)
}

pub async fn delete_credential_meta(pool: &Pool, id: &str) -> Result<Option<(String, String)>> {
    let row =
        sqlx::query("SELECT keychain_service, keychain_account FROM credentials WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    let Some(r) = row else { return Ok(None) };
    let pair = (
        r.try_get("keychain_service")?,
        r.try_get("keychain_account")?,
    );
    sqlx::query("DELETE FROM credentials WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(Some(pair))
}

// -------------------------------------------------------------- api tokens --

pub async fn insert_token(pool: &Pool, name: &str, hash: &str, scopes: &str) -> Result<String> {
    let id = crate::new_id();
    sqlx::query(
        "INSERT INTO api_tokens (id, name, token_hash, scopes, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(name)
    .bind(hash)
    .bind(scopes)
    .bind(crate::now_iso())
    .execute(pool)
    .await?;
    Ok(id)
}

pub struct TokenRow {
    pub id: String,
    pub name: String,
    pub scopes: String,
}

/// Look a token up by its hash. Callers hash the presented bearer first, so the
/// plaintext never reaches the database layer.
pub async fn token_by_hash(pool: &Pool, hash: &str) -> Result<Option<TokenRow>> {
    let row = sqlx::query(
        "SELECT id, name, scopes FROM api_tokens WHERE token_hash = ? AND revoked_at IS NULL",
    )
    .bind(hash)
    .fetch_optional(pool)
    .await?;
    match row {
        Some(r) => Ok(Some(TokenRow {
            id: r.try_get("id")?,
            name: r.try_get("name")?,
            scopes: r.try_get("scopes")?,
        })),
        None => Ok(None),
    }
}

pub async fn touch_token(pool: &Pool, id: &str) -> Result<()> {
    sqlx::query("UPDATE api_tokens SET last_used_at = ? WHERE id = ?")
        .bind(crate::now_iso())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn has_any_token(pool: &Pool) -> Result<bool> {
    let row = sqlx::query("SELECT COUNT(*) AS n FROM api_tokens WHERE revoked_at IS NULL")
        .fetch_one(pool)
        .await?;
    Ok(row.try_get::<i64, _>("n")? > 0)
}

/// One row of "every call another app made to your agent".
///
/// Bodies are deliberately not recorded: the audit trail should answer who did
/// what, without becoming a second copy of your data.
pub struct AuditEntry<'a> {
    pub token_id: Option<&'a str>,
    pub token_name: Option<&'a str>,
    pub remote_ip: &'a str,
    pub method: &'a str,
    pub path: &'a str,
    pub status: u16,
    pub latency_ms: i64,
    pub request_id: &'a str,
}

pub async fn record_audit(pool: &Pool, e: AuditEntry<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO api_audit (ts, token_id, token_name, remote_ip, method, path,
                                status, latency_ms, request_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(crate::now_iso())
    .bind(e.token_id)
    .bind(e.token_name)
    .bind(e.remote_ip)
    .bind(e.method)
    .bind(e.path)
    .bind(e.status as i64)
    .bind(e.latency_ms)
    .bind(e.request_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Pause a task because the system decided to, not because the user did.
/// Distinguished from a user pause in the UI so the reason is visible.
/// How many of this task's most recent finished runs failed, one after another.
///
/// The loop nothing bounded. Every ceiling in the app is a ceiling on ONE run:
/// steps, minutes, money, heal cycles, turns. A task on an hourly schedule
/// whose site has changed fails inside every one of those ceilings and then
/// does it again in an hour, for ever, and each failure is individually
/// well behaved.
///
/// Counts backwards from the newest and stops at the first run that did not
/// fail, so one success genuinely clears the count rather than merely diluting
/// it. Runs still in flight are not counted: an unfinished run has not failed.
/// Make the plan a run just wrote the one this task follows, where that is safe.
///
/// The rule, which is narrower than it looks and deliberately so: a playbook a
/// run wrote becomes active only when the task had none at all, the run was
/// real rather than a rehearsal, and the run succeeded.
///
/// The narrowness is the point. A playbook is distilled from pages written by
/// strangers and then handed back to the agent as trusted instruction;
/// scrubbing removes secrets, not instructions. Letting a later run replace a
/// plan that is already in force would turn one run against a changed or
/// hostile page into standing orders for every unattended run afterwards. A
/// revision therefore waits, and a person reads it: revising a document
/// somebody already relied on is exactly the case where review is worth
/// something.
///
/// Returns the version that became active, if any.
pub async fn adopt_plan_written_by(
    pool: &Pool,
    task_id: &str,
    run_id: &str,
) -> Result<Option<i64>> {
    let Some(task) = get_task(pool, task_id).await? else {
        return Ok(None);
    };
    if task.playbook_version.is_some() {
        return Ok(None);
    }
    let row = sqlx::query(
        "SELECT version FROM playbook_versions
         WHERE task_id = ? AND created_by_run_id = ?
         ORDER BY version DESC LIMIT 1",
    )
    .bind(task_id)
    .bind(run_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let version: i64 = row.try_get("version")?;
    set_active_playbook(pool, task_id, version).await?;
    Ok(Some(version))
}

/// What this run has already committed to spending, in dollars.
///
/// Read back out of the fence's own evidence rather than counted separately,
/// because the fence is the record of what really happened: a purchase that was
/// armed and never confirmed is not spending that can be undone by forgetting
/// it. Anything unparseable counts as nothing, which is the wrong way round for
/// safety, so the caller must treat an unknown amount as a refusal rather than
/// as zero.
pub async fn spent_so_far(pool: &Pool, run_id: &str) -> Result<f64> {
    let rows = sqlx::query(
        "SELECT evidence_json FROM side_effects
         WHERE run_id = ? AND action_kind = 'purchase' AND state = 'committed'",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await?;
    let mut total = 0.0;
    for r in rows {
        let raw: Option<String> = r.try_get("evidence_json")?;
        let Some(raw) = raw else { continue };
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
            total += v.get("amount_usd").and_then(|a| a.as_f64()).unwrap_or(0.0);
        }
    }
    Ok(total)
}

/// Has this task ever really done the job?
///
/// The evidence that replaced "a person approved a plan" as the thing standing
/// between a task and an unattended schedule. Proven beats reviewed, but only
/// if the proof was real: a rehearsal is told to carry on as though everything
/// worked and lands in the same 'succeeded' column having touched nothing, so
/// it is excluded here by the same predicate the run reader uses. The mode
/// clause catches rows written before the flag existed, and a teach run that
/// was also a rehearsal stores mode 'teach' with the flag set, which is why
/// both halves are needed.
pub async fn has_really_worked_once(pool: &Pool, task_id: &str) -> Result<bool> {
    let row = sqlx::query(
        "SELECT 1 FROM runs
         WHERE task_id = ? AND status = 'succeeded'
           AND rehearsal = 0 AND mode <> 'dry_run'
         LIMIT 1",
    )
    .bind(task_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

pub async fn consecutive_failures(pool: &Pool, task_id: &str) -> Result<i64> {
    // Only runs that actually tried. The scheduler manufactures 'skipped' runs
    // of its own for a task that was still running, collided with a catch-up,
    // or missed its window, and a task on a short cron produces those between
    // its failures. Counting them as "not a failure" would silently reset the
    // count on exactly the tasks that fire most often, which is to say the ones
    // this ceiling exists for. 'cancelled' is left out for the same reason: a
    // person stopping a run is not the task working.
    let rows = sqlx::query(
        "SELECT status FROM runs
         WHERE task_id = ? AND status IN ('succeeded','failed')
         ORDER BY created_at DESC, id DESC
         LIMIT 50",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await?;
    let mut n = 0i64;
    for r in rows {
        if r.try_get::<String, _>("status")? == "failed" {
            n += 1;
        } else {
            break;
        }
    }
    Ok(n)
}

pub async fn auto_pause_task(pool: &Pool, task_id: &str, reason: &str) -> Result<()> {
    sqlx::query(
        "UPDATE tasks SET status = 'paused', auto_paused = 1, paused_reason = ?, updated_at = ?
         WHERE id = ? AND status IN ('ready','paused')",
    )
    .bind(reason)
    .bind(crate::now_iso())
    .bind(task_id)
    .execute(pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------- browsers --

/// Claim the browser profile for a site identity, creating it if needed.
///
/// One Chromium profile serves one process at a time, so the claim is a hard
/// mutex. The lock is tied to the run rather than left dangling: crash recovery
/// releases any lock whose run is no longer live, otherwise one crashed run
/// locks a site out permanently and the only fix is editing the database.
pub async fn claim_browser_profile(
    pool: &Pool,
    apex_domain: &str,
    run_id: &str,
) -> Result<(String, String)> {
    let name = format!("{apex_domain} (default)");
    let existing =
        sqlx::query("SELECT id, dir_name, locked_by_run FROM browser_profiles WHERE name = ?")
            .bind(&name)
            .fetch_optional(pool)
            .await?;

    let (id, dir_name) = match existing {
        Some(r) => {
            let id: String = r.try_get("id")?;
            let dir: String = r.try_get("dir_name")?;
            let holder: Option<String> = r.try_get("locked_by_run")?;
            if let Some(h) = holder {
                if h != run_id && run_is_live(pool, &h).await? {
                    anyhow::bail!(
                        "The browser profile for {apex_domain} is in use by another run. \
                         Only one run at a time can hold a site's logged-in session."
                    );
                }
            }
            (id, dir)
        }
        None => {
            let id = crate::new_id();
            let dir = format!("profiles/{id}");
            sqlx::query(
                "INSERT INTO browser_profiles (id, name, dir_name, default_domain, created_at)
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(&name)
            .bind(&dir)
            .bind(apex_domain)
            .bind(crate::now_iso())
            .execute(pool)
            .await?;
            (id, dir)
        }
    };

    sqlx::query("UPDATE browser_profiles SET locked_by_run = ?, last_used_at = ? WHERE id = ?")
        .bind(run_id)
        .bind(crate::now_iso())
        .bind(&id)
        .execute(pool)
        .await?;
    Ok((id, dir_name))
}

/// Is this run still in a state where it could be holding resources?
async fn run_is_live(pool: &Pool, run_id: &str) -> Result<bool> {
    let row = sqlx::query(
        "SELECT COUNT(*) AS n FROM runs WHERE id = ? AND status IN
         ('armed','queued','preflight','holding','running','healing','waiting_input','takeover')",
    )
    .bind(run_id)
    .fetch_one(pool)
    .await?;
    Ok(row.try_get::<i64, _>("n")? > 0)
}

pub async fn release_browser_profiles(pool: &Pool, run_id: &str) -> Result<()> {
    sqlx::query("UPDATE browser_profiles SET locked_by_run = NULL WHERE locked_by_run = ?")
        .bind(run_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Credentials a task may use, with the domain each is bound to.
pub async fn credentials_for_task(
    pool: &Pool,
    task_id: &str,
) -> Result<Vec<crate::models::CredentialMeta>> {
    let rows = sqlx::query(
        "SELECT c.* FROM credentials c
         JOIN task_credentials tc ON tc.credential_id = c.id
         WHERE tc.task_id = ? ORDER BY c.label",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(crate::models::CredentialMeta {
                id: r.try_get("id")?,
                label: r.try_get("label")?,
                kind: r.try_get("kind")?,
                domain: r.try_get("domain")?,
                username: r.try_get("username")?,
                require_biometric: r.try_get::<i64, _>("require_biometric")? != 0,
                last_used_at: r.try_get("last_used_at")?,
                use_count: r.try_get("use_count")?,
                created_at: r.try_get("created_at")?,
            })
        })
        .collect()
}

pub async fn link_task_credential(pool: &Pool, task_id: &str, credential_id: &str) -> Result<()> {
    sqlx::query("INSERT OR IGNORE INTO task_credentials (task_id, credential_id) VALUES (?, ?)")
        .bind(task_id)
        .bind(credential_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Stop a task using a login. Always safe: the login itself is untouched, and a
/// run that needed it will say so plainly rather than signing in as somebody.
pub async fn unlink_task_credential(
    pool: &Pool,
    task_id: &str,
    credential_id: &str,
) -> Result<bool> {
    Ok(
        sqlx::query("DELETE FROM task_credentials WHERE task_id = ? AND credential_id = ?")
            .bind(task_id)
            .bind(credential_id)
            .execute(pool)
            .await?
            .rows_affected()
            > 0,
    )
}

pub async fn credential_keychain_ref(
    pool: &Pool,
    cred_id: &str,
) -> Result<Option<(String, String, String)>> {
    let row = sqlx::query(
        "SELECT keychain_service, keychain_account, domain FROM credentials WHERE id = ?",
    )
    .bind(cred_id)
    .fetch_optional(pool)
    .await?;
    match row {
        Some(r) => Ok(Some((
            r.try_get("keychain_service")?,
            r.try_get("keychain_account")?,
            r.try_get("domain")?,
        ))),
        None => Ok(None),
    }
}

pub async fn mark_credential_used(pool: &Pool, cred_id: &str) -> Result<()> {
    sqlx::query("UPDATE credentials SET use_count = use_count + 1, last_used_at = ? WHERE id = ?")
        .bind(crate::now_iso())
        .bind(cred_id)
        .execute(pool)
        .await?;
    Ok(())
}

// -------------------------------------------------------------- recipients --

/// The ways Errand can send a message. Same vocabulary as the outbox and the
/// database CHECK, checked here so a wrong one is refused in a sentence rather
/// than as a constraint violation.
const CHANNELS: &[&str] = &["telegram", "whatsapp", "apple_mail", "imessage"];

/// An address with most of it taken out, for showing to the agent.
///
/// The point is recognition, not reach: enough for a person to confirm the
/// right contact was picked, not enough to write to them. The agent chooses
/// from a list of recipients the person granted; it never needs the address
/// itself, and an address it never sees is an address it cannot leak.
pub fn masked_address(channel: &str, address: &str) -> String {
    let a = address.trim();
    if a.is_empty() {
        return "•••".to_string();
    }
    let looks_like_email = a.contains('@') && !a.starts_with('@');
    match channel {
        "apple_mail" => mask_email(a),
        "telegram" if a.starts_with('@') => mask_handle(a),
        _ if looks_like_email => mask_email(a),
        _ => mask_number(a),
    }
}

fn mask_email(a: &str) -> String {
    let Some((local, domain)) = a.split_once('@') else {
        return mask_number(a);
    };
    if domain.is_empty() {
        return "•••".to_string();
    }
    match local.chars().next() {
        Some(first) => format!("{first}•••@{domain}"),
        None => format!("•••@{domain}"),
    }
}

fn mask_handle(a: &str) -> String {
    match a.trim_start_matches('@').chars().next() {
        Some(first) => format!("@{first}•••"),
        None => "@•••".to_string(),
    }
}

fn mask_number(a: &str) -> String {
    let digits: Vec<char> = a.chars().filter(|c| c.is_ascii_digit()).collect();
    // Too short to hide anything usefully, so hide all of it.
    if digits.len() < 4 {
        return "•••".to_string();
    }
    let last: String = digits[digits.len() - 2..].iter().collect();
    if a.starts_with('+') {
        let country: String = digits[..2].iter().collect();
        format!("+{country} ••• ••{last}")
    } else {
        format!("••• ••{last}")
    }
}

fn recipient_from_row(r: &sqlx::sqlite::SqliteRow) -> Result<crate::models::Recipient> {
    let channel: String = r.try_get("channel")?;
    let address: String = r.try_get("address")?;
    Ok(crate::models::Recipient {
        id: r.try_get("id")?,
        label: r.try_get("label")?,
        address_masked: masked_address(&channel, &address),
        channel,
        address,
        created_at: r.try_get("created_at")?,
    })
}

/// Add somebody Errand may write to. Global: granting a task access to them is
/// a separate, deliberate step.
pub async fn create_recipient(
    pool: &Pool,
    label: &str,
    channel: &str,
    address: &str,
) -> Result<String> {
    if !CHANNELS.contains(&channel) {
        anyhow::bail!(
            "'{channel}' is not a way Errand can send messages, so this contact was not saved. \
             Choose Telegram, WhatsApp, Mail or iMessage."
        );
    }
    if label.trim().is_empty() {
        anyhow::bail!("Give this contact a name you will recognise later, such as 'Mum'.");
    }
    if address.trim().is_empty() {
        anyhow::bail!(
            "This contact has no address, so nothing could ever be sent to them. Add the phone \
             number, email address or handle for the way you picked."
        );
    }

    let id = crate::new_id();
    sqlx::query(
        "INSERT INTO recipients (id, label, channel, address, created_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(label.trim())
    .bind(channel)
    .bind(address.trim())
    .bind(crate::now_iso())
    .execute(pool)
    .await?;
    Ok(id)
}

pub async fn list_recipients(pool: &Pool) -> Result<Vec<crate::models::Recipient>> {
    let rows = sqlx::query("SELECT * FROM recipients ORDER BY label")
        .fetch_all(pool)
        .await?;
    rows.iter().map(recipient_from_row).collect()
}

/// The people this one task may contact, and what it may tell them.
pub async fn recipients_for_task(
    pool: &Pool,
    task_id: &str,
) -> Result<Vec<crate::models::TaskRecipient>> {
    let rows = sqlx::query(
        "SELECT r.id, r.label, r.channel, r.address, tr.on_success, tr.on_failure
         FROM recipients r
         JOIN task_recipients tr ON tr.recipient_id = r.id
         WHERE tr.task_id = ? ORDER BY r.label",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|r| {
            let channel: String = r.try_get("channel")?;
            let address: String = r.try_get("address")?;
            Ok(crate::models::TaskRecipient {
                id: r.try_get("id")?,
                label: r.try_get("label")?,
                address_masked: masked_address(&channel, &address),
                channel,
                address,
                on_success: r.try_get::<i64, _>("on_success")? != 0,
                on_failure: r.try_get::<i64, _>("on_failure")? != 0,
            })
        })
        .collect()
}

/// Let a task contact somebody. Linking again updates the flags rather than
/// failing, so the settings screen can just write what the person chose.
pub async fn link_recipient(
    pool: &Pool,
    task_id: &str,
    recipient_id: &str,
    on_success: bool,
    on_failure: bool,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO task_recipients (task_id, recipient_id, on_success, on_failure)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(task_id, recipient_id) DO UPDATE
           SET on_success = excluded.on_success, on_failure = excluded.on_failure",
    )
    .bind(task_id)
    .bind(recipient_id)
    .bind(i64::from(on_success))
    .bind(i64::from(on_failure))
    .execute(pool)
    .await?;
    Ok(())
}

/// Take the grant away from one task. The contact itself stays, and every other
/// task that has them keeps them.
pub async fn unlink_recipient(pool: &Pool, task_id: &str, recipient_id: &str) -> Result<bool> {
    let res = sqlx::query("DELETE FROM task_recipients WHERE task_id = ? AND recipient_id = ?")
        .bind(task_id)
        .bind(recipient_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Remove a contact everywhere. Every task's grant goes with them, by cascade,
/// so no task is left holding a grant to somebody who no longer exists.
pub async fn delete_recipient(pool: &Pool, id: &str) -> Result<bool> {
    let res = sqlx::query("DELETE FROM recipients WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Revoke every live token with this name. Used by the token recovery path.
pub async fn revoke_tokens_named(pool: &Pool, name_prefix: &str) -> Result<u64> {
    let res = sqlx::query(
        "UPDATE api_tokens SET revoked_at = ? WHERE name LIKE ? AND revoked_at IS NULL",
    )
    .bind(crate::now_iso())
    .bind(format!("{name_prefix}%"))
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

// ------------------------------------------------------------- mail access --
//
// The same shape as the recipient grants above, and for the same reason. A
// task can reach the person's mail only because somebody sat down and said so
// for that one task; there is no global switch, and nothing a run reads can
// create one of these rows.

/// What one task is allowed to do with the person's mail.
///
/// The absence of this, rather than any field on it, is what refuses a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailGrant {
    /// May it move messages between mailboxes, as opposed to only reading them.
    pub may_file: bool,
    pub granted_at: String,
}

/// Let one task read the person's mail, and optionally tidy it.
///
/// Granting again updates the filing half rather than failing, so the task
/// screen can simply write what the person chose.
pub async fn grant_mail(pool: &Pool, task_id: &str, may_file: bool) -> Result<()> {
    sqlx::query(
        "INSERT INTO task_mail_grants (task_id, may_file, granted_at)
         VALUES (?, ?, ?)
         ON CONFLICT(task_id) DO UPDATE SET may_file = excluded.may_file",
    )
    .bind(task_id)
    .bind(i64::from(may_file))
    .bind(crate::now_iso())
    .execute(pool)
    .await?;
    Ok(())
}

/// Take the grant away. The next run of this task cannot see the mail tools at
/// all, and messages already read or moved are not affected by it.
pub async fn revoke_mail(pool: &Pool, task_id: &str) -> Result<bool> {
    let res = sqlx::query("DELETE FROM task_mail_grants WHERE task_id = ?")
        .bind(task_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// What this task may do with the mail. `None` means it was never granted, so
/// it may do nothing at all.
pub async fn mail_grant_for_task(pool: &Pool, task_id: &str) -> Result<Option<MailGrant>> {
    let row = sqlx::query("SELECT may_file, granted_at FROM task_mail_grants WHERE task_id = ?")
        .bind(task_id)
        .fetch_optional(pool)
        .await?;
    match row {
        Some(r) => Ok(Some(MailGrant {
            may_file: r.try_get::<i64, _>("may_file")? != 0,
            granted_at: r.try_get("granted_at")?,
        })),
        None => Ok(None),
    }
}

// ------------------------------------------------------------ side effects --

/// What the fence says about an irreversible action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FenceVerdict {
    /// Go ahead. Call `commit_side_effect` with evidence once it is done.
    Armed(String),
    /// This occurrence already did this. Do not do it again.
    AlreadyCommitted { evidence: Option<String> },
    /// A previous attempt armed this and never reported back, so nobody knows
    /// whether it actually happened. Verify on the site before arming again.
    NeedsVerification { armed_at: String },
}

/// The fence key.
///
/// Scope folds in only when it is non-empty, so every key written before scopes
/// existed still comes out byte-identical. That is not tidiness: if a committed
/// booking's key changed shape, the fence would stop recognising it, and the
/// next attempt would read an already-taken slot as free and book it twice.
fn fence_key(task_id: &str, occurrence_id: &str, action_kind: &str, scope: &str) -> String {
    if scope.is_empty() {
        format!("{task_id}:{occurrence_id}:{action_kind}")
    } else {
        format!("{task_id}:{occurrence_id}:{action_kind}:{scope}")
    }
}

/// Ask the fence for permission to do something irreversible.
///
/// The key is scoped to the occurrence, never to what the agent chose to do. A
/// key like `book:court2:19:00` would let a retry that picks court 4 straight
/// past the guard and double-book, which is the exact failure the fence exists
/// to prevent. One scheduled slot admits one irreversible action, whatever the
/// agent decides that action should be.
///
/// `scope` is the one exception, and it does not weaken that rule. The warning
/// above is about outcomes the AGENT picks: key on one of those and a retry
/// simply picks differently and walks through. A scope is picked by the PERSON,
/// from a closed set the agent cannot add to (a recipient they chose, say), so
/// "message this person once for this slot" is a promise the agent has no way
/// to reinterpret. Pass an empty scope when there is no such set, which is
/// most of the time.
pub async fn arm_side_effect(
    pool: &Pool,
    run_id: &str,
    task_id: &str,
    occurrence_id: &str,
    action_kind: &str,
    scope: &str,
) -> Result<FenceVerdict> {
    let key = fence_key(task_id, occurrence_id, action_kind, scope);
    let id = crate::new_id();
    let now = crate::now_iso();

    // One statement, so two callers cannot both read "aborted" and both take
    // the slot. A check followed by a separate write would hand two agents a
    // valid go-ahead for the same booking, which is the failure this whole
    // mechanism exists to prevent.
    //
    // The conflict clause claims the row only when it is currently aborted; a
    // committed or already-armed row is left untouched and reported back as it
    // stands.
    let row = sqlx::query(
        "INSERT INTO side_effects (id, run_id, task_id, occurrence_id, action_kind,
                                   scope, idempotency_key, state, armed_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, 'armed', ?)
         ON CONFLICT(idempotency_key) DO UPDATE
           SET state = 'armed', run_id = excluded.run_id, armed_at = excluded.armed_at
           WHERE side_effects.state = 'aborted'
         RETURNING id, state, evidence_json, armed_at",
    )
    .bind(&id)
    .bind(run_id)
    .bind(task_id)
    .bind(occurrence_id)
    .bind(action_kind)
    .bind(scope)
    .bind(&key)
    .bind(&now)
    .fetch_optional(pool)
    .await?;

    if let Some(r) = row {
        // We either inserted or claimed an aborted row; either way it is ours.
        return Ok(FenceVerdict::Armed(r.try_get("id")?));
    }

    // The conflict clause declined to update, so an existing row is committed
    // or already armed. Read it to say which.
    let r = sqlx::query(
        "SELECT id, state, evidence_json, armed_at FROM side_effects WHERE idempotency_key = ?",
    )
    .bind(&key)
    .fetch_one(pool)
    .await?;
    let state: String = r.try_get("state")?;
    Ok(match state.as_str() {
        "committed" => FenceVerdict::AlreadyCommitted {
            evidence: r.try_get("evidence_json")?,
        },
        _ => FenceVerdict::NeedsVerification {
            armed_at: r.try_get("armed_at")?,
        },
    })
}

/// Record that the irreversible thing actually happened, with proof.
pub async fn commit_side_effect(pool: &Pool, id: &str, evidence: &str) -> Result<()> {
    let res = sqlx::query(
        "UPDATE side_effects SET state = 'committed', committed_at = ?, evidence_json = ?
         WHERE id = ? AND state = 'armed'",
    )
    .bind(crate::now_iso())
    .bind(evidence)
    .bind(id)
    .execute(pool)
    .await?;
    // A silent no-op here is the worst possible outcome: the caller believes
    // the irreversible action was recorded, the evidence is lost, and the next
    // attempt sees a slot that looks free.
    if res.rows_affected() != 1 {
        let state: Option<String> = sqlx::query("SELECT state FROM side_effects WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?
            .map(|r| r.try_get("state"))
            .transpose()?;
        anyhow::bail!(
            "could not record that the action completed: fence {id} is {}, not armed",
            state.unwrap_or_else(|| "missing".into())
        );
    }
    Ok(())
}

/// Record that it was decided against, freeing the slot for a later attempt.
pub async fn abort_side_effect(pool: &Pool, id: &str, why: &str) -> Result<()> {
    let res = sqlx::query(
        "UPDATE side_effects SET state = 'aborted', evidence_json = ? WHERE id = ? AND state = 'armed'",
    )
    .bind(why)
    .bind(id)
    .execute(pool)
    .await?;
    if res.rows_affected() != 1 {
        anyhow::bail!("fence {id} was not armed, so it could not be released");
    }
    Ok(())
}

/// Any fence left armed by a run that is no longer live.
///
/// A crash between arming and committing means nobody knows whether the action
/// happened. Such a run is never retried automatically, and a manual retry
/// enters verify-first mode.
pub async fn dangling_fences(pool: &Pool, task_id: &str, occurrence_id: &str) -> Result<bool> {
    let row = sqlx::query(
        "SELECT COUNT(*) AS n FROM side_effects
         WHERE task_id = ? AND occurrence_id = ? AND state = 'armed'",
    )
    .bind(task_id)
    .bind(occurrence_id)
    .fetch_one(pool)
    .await?;
    Ok(row.try_get::<i64, _>("n")? > 0)
}

/// Record when this task next comes round, or that it does not come round again.
///
/// `None` is a real answer and writes NULL. Without it "there is no next run"
/// could not be recorded at all, so a one-off that has been and gone kept the
/// time it ran at sitting in its next-run field, and the task page went on
/// promising a run in the past for as long as the task existed. Callers should
/// pass whatever they computed and let it be written either way, rather than
/// skipping the write when there is nothing to say.
pub async fn set_next_run_at(pool: &Pool, task_id: &str, iso: Option<&str>) -> Result<()> {
    sqlx::query("UPDATE tasks SET next_run_at = ? WHERE id = ?")
        .bind(iso)
        .bind(task_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Close a run that deliberately did not happen. Skipped is not a failure, so
/// it carries an explanation but no failure code the retry ladder would act on.
pub async fn finish_run_skipped(pool: &Pool, run_id: &str, code: &str, human: &str) -> Result<()> {
    sqlx::query(
        "UPDATE runs SET status = 'skipped', finished_at = ?, summary_md = ?, notes_md = ?
         WHERE id = ?",
    )
    .bind(crate::now_iso())
    .bind(human)
    .bind(code)
    .bind(run_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// The most recent committed action of this kind for this task, if it happened
/// within the given window.
///
/// The per-occurrence fence protects a scheduled slot, but a manual "Run now"
/// mints a fresh slot every time, so nothing stops someone triggering a booking
/// task twice in a minute and booking twice. This is what makes that visible.
///
/// `scope` narrows it to one recipient, the same way it narrows the fence key.
/// An empty scope is not a wildcard: it matches only the rows written with no
/// scope at all. Ask this when the question really is about one person ("has
/// Mum already been told") and `recent_commit_of_any_scope` when it is about
/// the task as a whole.
pub async fn recent_commit(
    pool: &Pool,
    task_id: &str,
    action_kind: &str,
    scope: &str,
    within_minutes: i64,
) -> Result<Option<(String, String, Option<String>)>> {
    let row = sqlx::query(
        "SELECT occurrence_id, committed_at, evidence_json FROM side_effects
         WHERE task_id = ? AND action_kind = ? AND scope = ? AND state = 'committed'
           AND committed_at IS NOT NULL
           AND committed_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?)
         ORDER BY committed_at DESC LIMIT 1",
    )
    .bind(task_id)
    .bind(action_kind)
    .bind(scope)
    .bind(format!("-{within_minutes} minutes"))
    .fetch_optional(pool)
    .await?;
    match row {
        Some(r) => Ok(Some((
            r.try_get("occurrence_id")?,
            r.try_get("committed_at")?,
            r.try_get("evidence_json")?,
        ))),
        None => Ok(None),
    }
}

/// The most recent committed action of this kind for this task, whoever or
/// whatever it was aimed at.
///
/// The sibling of `recent_commit`, and both exist because the two questions are
/// genuinely different. "Has Mum already been told about this?" is scoped to
/// Mum, and answering it across every recipient would silence a message to
/// somebody else. "Has this task done anything irreversible of this kind
/// lately?" is not scoped to anybody, and answering it with an empty scope
/// finds nothing at all: every message is armed with the recipient's id, so the
/// empty scope matches none of them. That is how a guard on schedule changes
/// came to look at the one column that could never match, and waved through
/// exactly the repeats it existed to catch.
///
/// Returns the scope alongside the rest, so a caller that wants to name the
/// person in what it says can look them up.
pub async fn recent_commit_of_any_scope(
    pool: &Pool,
    task_id: &str,
    action_kind: &str,
    within_minutes: i64,
) -> Result<Option<(String, String, Option<String>, String)>> {
    let row = sqlx::query(
        "SELECT occurrence_id, committed_at, evidence_json, scope FROM side_effects
         WHERE task_id = ? AND action_kind = ? AND state = 'committed'
           AND committed_at IS NOT NULL
           AND committed_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?)
         ORDER BY committed_at DESC LIMIT 1",
    )
    .bind(task_id)
    .bind(action_kind)
    .bind(format!("-{within_minutes} minutes"))
    .fetch_optional(pool)
    .await?;
    match row {
        Some(r) => Ok(Some((
            r.try_get("occurrence_id")?,
            r.try_get("committed_at")?,
            r.try_get("evidence_json")?,
            r.try_get("scope")?,
        ))),
        None => Ok(None),
    }
}

/// Put a task away: gone from the list, never runs again, history kept.
///
/// The default way to get rid of one, and not the same as deleting it. A task
/// that has booked, bought or filed something leaves rows in the side-effect
/// record, and those rows are what stop a future run doing the same thing
/// twice. Throwing them away to tidy a list is throwing away the evidence.
///
/// Reversible by hand, because "I did not mean that one" is a thing people say.
pub async fn archive_task(pool: &Pool, task_id: &str) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE tasks SET status = 'archived', updated_at = ? WHERE id = ? AND status <> 'archived'",
    )
    .bind(crate::now_iso())
    .bind(task_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Really remove a task and everything it left behind.
///
/// For a task that never should have existed: a test, a mistake, a duplicate.
/// Says how many runs went with it, because that is the number somebody wants
/// to have been told before it is gone rather than after.
///
/// Refuses while a run is in flight. Deleting the row under a running agent
/// leaves it writing steps to a task that is not there.
pub async fn forget_task(pool: &Pool, task_id: &str) -> Result<i64> {
    if busy_run_for_task(pool, task_id).await?.is_some() {
        anyhow::bail!(
            "That task is running right now. Wait for it to finish, or stop it, then remove it."
        );
    }
    let runs: i64 = sqlx::query("SELECT COUNT(*) AS n FROM runs WHERE task_id = ?")
        .bind(task_id)
        .fetch_one(pool)
        .await?
        .try_get("n")?;

    // In one transaction, so a half-removed task cannot be left behind: rows
    // pointing at a task that no longer exists are worse than the task was.
    let mut tx = pool.begin().await?;
    // Written out rather than looped: sqlx refuses a SQL string built at
    // runtime, which is the right refusal, and naming each table here means a
    // table that gets renamed breaks the build instead of quietly leaving rows
    // behind that point at a task which is gone.
    sqlx::query("DELETE FROM run_steps WHERE run_id IN (SELECT id FROM runs WHERE task_id = ?)")
        .bind(task_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "DELETE FROM run_artifacts WHERE run_id IN (SELECT id FROM runs WHERE task_id = ?)",
    )
    .bind(task_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "DELETE FROM run_answer_copies WHERE run_id IN (SELECT id FROM runs WHERE task_id = ?)",
    )
    .bind(task_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM side_effects WHERE task_id = ?")
        .bind(task_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM playbook_versions WHERE task_id = ?")
        .bind(task_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM task_recipients WHERE task_id = ?")
        .bind(task_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM task_mail_grants WHERE task_id = ?")
        .bind(task_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM task_credentials WHERE task_id = ?")
        .bind(task_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM msg_outbox WHERE task_id = ?")
        .bind(task_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM runs WHERE task_id = ?")
        .bind(task_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM tasks WHERE id = ?")
        .bind(task_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(runs)
}

/// Let a task run itself again, after a run of it worked.
///
/// Only ever clears a pause Errand set for itself. A pause somebody set by hand
/// means "stop doing this", and a task quietly starting again because one run
/// happened to work would be the program overruling them.
///
/// Returns whether anything changed, so the caller can say so.
pub async fn clear_auto_pause(pool: &Pool, task_id: &str) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE tasks SET status = 'ready', auto_paused = 0, paused_reason = NULL, updated_at = ?
         WHERE id = ? AND auto_paused = 1 AND status = 'paused'",
    )
    .bind(crate::now_iso())
    .bind(task_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Move a task from draft to ready so the scheduler will consider it.
///
/// Without this there is no path at all from creating a task to it ever
/// running, which is exactly the sort of gap that hides when tests reach into
/// the database directly instead of going through the product.
pub async fn activate_task(pool: &Pool, task_id: &str) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE tasks SET status = 'ready', auto_paused = 0, paused_reason = NULL, updated_at = ?
         WHERE id = ? AND status IN ('draft', 'teaching', 'ready')",
    )
    .bind(crate::now_iso())
    .bind(task_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Is this task already running something?
pub async fn busy_run_for_task(pool: &Pool, task_id: &str) -> Result<Option<String>> {
    let row = sqlx::query(
        "SELECT id FROM runs WHERE task_id = ? AND status IN
         ('armed','queued','preflight','holding','running','healing','waiting_input','takeover')
         LIMIT 1",
    )
    .bind(task_id)
    .fetch_optional(pool)
    .await?;
    match row {
        Some(r) => Ok(Some(r.try_get("id")?)),
        None => Ok(None),
    }
}

/// Close out runs left mid-flight by a daemon that died.
///
/// Without this a killed run stays "running" forever: it never ends, never
/// explains itself, and permanently inflates the busy count. Returns the runs
/// it closed, with a flag for those that were holding an unresolved
/// irreversible action, because those tasks must not simply carry on.
pub async fn recover_interrupted_runs(pool: &Pool) -> Result<Vec<(String, String, bool)>> {
    let rows = sqlx::query(
        "SELECT id, task_id FROM runs WHERE status IN
         ('armed','queued','preflight','holding','running','healing','waiting_input','takeover')",
    )
    .fetch_all(pool)
    .await?;

    let mut out = vec![];
    for r in rows {
        let run_id: String = r.try_get("id")?;
        let task_id: String = r.try_get("task_id")?;

        let armed = sqlx::query(
            "SELECT COUNT(*) AS n FROM side_effects WHERE run_id = ? AND state = 'armed'",
        )
        .bind(&run_id)
        .fetch_one(pool)
        .await?
        .try_get::<i64, _>("n")?
            > 0;

        let human = if armed {
            concat!(
                "**What I was doing:** Something that cannot be undone, such as booking or ",
                "sending.\n",
                "**Why I could not finish:** Errand stopped while that was in progress, so ",
                "nobody knows whether it went through.\n",
                "**What you can do:** Check the site before running this again. This task has ",
                "been paused so it cannot repeat the action by accident."
            )
            .to_string()
        } else {
            concat!(
                "**What I was doing:** Working on this task.\n",
                "**Why I could not finish:** Errand stopped while the run was in progress, so ",
                "it never got to report what it had done.\n",
                "**What you can do:** Check whether anything was completed, then press Run now."
            )
            .to_string()
        };

        finish_run_failed(pool, &run_id, "interrupted", &human, None).await?;
        if armed {
            auto_pause_task(pool, &task_id, "interrupted_mid_action").await?;
        }
        out.push((run_id, task_id, armed));
    }
    Ok(out)
}

/// Release every unresolved irreversible action on a task.
///
/// A run that armed a fence and died leaves the task blocked: the scheduler
/// will not fire it and Run now refuses, which is correct, but without this
/// there is no way out except editing the database. The user checks the site,
/// then tells Errand what they found.
pub async fn clear_holds(pool: &Pool, task_id: &str, note: &str) -> Result<u64> {
    let res = sqlx::query(
        "UPDATE side_effects SET state = 'aborted', evidence_json = ?
         WHERE task_id = ? AND state = 'armed'",
    )
    .bind(note)
    .bind(task_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Mark an unresolved action as having actually happened, on the user's word.
pub async fn confirm_holds(pool: &Pool, task_id: &str, note: &str) -> Result<u64> {
    let res = sqlx::query(
        "UPDATE side_effects SET state = 'committed', committed_at = ?, evidence_json = ?
         WHERE task_id = ? AND state = 'armed'",
    )
    .bind(crate::now_iso())
    .bind(note)
    .bind(task_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// How many armed side effects block one task right now.
///
/// The interface keys its "needs you" card off this number rather than off
/// the wording of the pause reason, which is a sentence that can change.
pub async fn count_open_holds(pool: &Pool, task_id: &str) -> Result<i64> {
    let row =
        sqlx::query("SELECT COUNT(*) AS n FROM side_effects WHERE task_id = ? AND state = 'armed'")
            .bind(task_id)
            .fetch_one(pool)
            .await?;
    Ok(row.try_get("n")?)
}

/// The same count for every task at once, so a list page does not ask one
/// task at a time.
pub async fn open_hold_counts(pool: &Pool) -> Result<std::collections::HashMap<String, i64>> {
    let rows = sqlx::query(
        "SELECT task_id, COUNT(*) AS n FROM side_effects WHERE state = 'armed' GROUP BY task_id",
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|r| Ok((r.try_get("task_id")?, r.try_get("n")?)))
        .collect()
}

// --------------------------------------------------------------- playbooks --

#[derive(Debug, Clone)]
pub struct PlaybookVersion {
    pub version: i64,
    pub source: String,
    pub approved: bool,
    pub changelog: Option<String>,
    pub created_by_run_id: Option<String>,
    pub created_at: String,
    pub sha256: String,
}

/// Store a new playbook version. Unapproved by default: nothing the agent
/// wrote about a site takes effect until a person has read it.
/// Did this run write a plan of its own?
///
/// Asked before distilling one from the journal, so the agent's own account of
/// what it was trying to do always wins over anything inferred afterwards.
pub async fn playbook_written_by_run(pool: &Pool, run_id: &str) -> Result<bool> {
    let row = sqlx::query("SELECT 1 FROM playbook_versions WHERE created_by_run_id = ? LIMIT 1")
        .bind(run_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some())
}

pub async fn add_playbook_version(
    pool: &Pool,
    task_id: &str,
    pb: &crate::playbook::Playbook,
    source: crate::playbook::Source,
    created_by_run_id: Option<&str>,
    changelog: Option<&str>,
    approved: bool,
) -> Result<i64> {
    let (path, sha) = crate::playbook::write(task_id, pb)?;
    let rel = path
        .strip_prefix(crate::paths::data_root()?)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string_lossy().to_string());

    sqlx::query(
        "INSERT INTO playbook_versions
           (task_id, version, rel_path, sha256, source, created_by_run_id, approved,
            changelog_md, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(task_id)
    .bind(pb.version)
    .bind(&rel)
    .bind(&sha)
    .bind(source.as_str())
    .bind(created_by_run_id)
    .bind(i64::from(approved))
    .bind(changelog)
    .bind(crate::now_iso())
    .execute(pool)
    .await?;

    if approved {
        set_active_playbook(pool, task_id, pb.version).await?;
    }
    Ok(pb.version)
}

pub async fn next_playbook_version(pool: &Pool, task_id: &str) -> Result<i64> {
    let row = sqlx::query(
        "SELECT COALESCE(MAX(version), 0) + 1 AS next FROM playbook_versions WHERE task_id = ?",
    )
    .bind(task_id)
    .fetch_one(pool)
    .await?;
    Ok(row.try_get("next")?)
}

pub async fn set_active_playbook(pool: &Pool, task_id: &str, version: i64) -> Result<()> {
    sqlx::query("UPDATE playbook_versions SET approved = 1 WHERE task_id = ? AND version = ?")
        .bind(task_id)
        .bind(version)
        .execute(pool)
        .await?;
    sqlx::query("UPDATE tasks SET active_playbook_version = ?, updated_at = ? WHERE id = ?")
        .bind(version)
        .bind(crate::now_iso())
        .bind(task_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn list_playbook_versions(pool: &Pool, task_id: &str) -> Result<Vec<PlaybookVersion>> {
    let rows = sqlx::query(
        "SELECT version, source, approved, changelog_md, created_by_run_id, created_at, sha256
         FROM playbook_versions WHERE task_id = ? ORDER BY version DESC",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(PlaybookVersion {
                version: r.try_get("version")?,
                source: r.try_get("source")?,
                approved: r.try_get::<i64, _>("approved")? != 0,
                changelog: r.try_get("changelog_md")?,
                created_by_run_id: r.try_get("created_by_run_id")?,
                created_at: r.try_get("created_at")?,
                sha256: r.try_get("sha256")?,
            })
        })
        .collect()
}

/// The playbook a run should follow: the approved one, or nothing.
pub async fn active_playbook(
    pool: &Pool,
    task_id: &str,
) -> Result<Option<crate::playbook::Playbook>> {
    let Some(task) = get_task(pool, task_id).await? else {
        return Ok(None);
    };
    let Some(v) = task.playbook_version else {
        return Ok(None);
    };
    Ok(crate::playbook::read(task_id, v).ok())
}

/// Notes left by recent runs, newest last, for the next run to read.
pub async fn recent_notes(pool: &Pool, task_id: &str, limit: i64) -> Result<Vec<String>> {
    let rows = sqlx::query(
        "SELECT notes_md FROM runs
         WHERE task_id = ? AND notes_md IS NOT NULL AND notes_md <> ''
           AND status IN ('succeeded','failed')
         ORDER BY created_at DESC, id DESC LIMIT ?",
    )
    .bind(task_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let mut notes: Vec<String> = rows
        .into_iter()
        .map(|r| r.try_get::<String, _>("notes_md"))
        .collect::<std::result::Result<_, _>>()?;
    notes.reverse();
    Ok(notes)
}

pub async fn set_run_notes(pool: &Pool, run_id: &str, notes: &str) -> Result<()> {
    sqlx::query("UPDATE runs SET notes_md = ? WHERE id = ?")
        .bind(notes)
        .bind(run_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_task_status(pool: &Pool, task_id: &str, status: &str) -> Result<()> {
    sqlx::query("UPDATE tasks SET status = ?, updated_at = ? WHERE id = ?")
        .bind(status)
        .bind(crate::now_iso())
        .bind(task_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ------------------------------------------------------------------ outbox --

pub struct NewMessage {
    pub run_id: Option<String>,
    pub task_id: Option<String>,
    pub class: String,
    pub channel: String,
    pub recipient: String,
    pub recipient_label: Option<String>,
    pub subject: Option<String>,
    pub body: String,
    pub is_failure: bool,
}

#[derive(Debug, Clone)]
pub struct OutboxRow {
    pub id: String,
    pub channel: String,
    pub class: String,
    pub recipient: String,
    pub subject: Option<String>,
    pub body: String,
    pub attempts: i64,
    pub is_failure: bool,
}

fn body_hash(channel: &str, recipient: &str, body: &str) -> String {
    let mut h: u64 = 1469598103934665603;
    for b in format!("{channel}|{recipient}|{body}").bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    format!("{h:016x}")
}

/// Queue a message. Identical messages to the same recipient within a few
/// minutes are dropped, because the usual cause is a bug rather than an
/// intention, and the person on the other end does not want it twice.
pub async fn enqueue_message(pool: &Pool, m: NewMessage) -> Result<Option<String>> {
    let hash = body_hash(&m.channel, &m.recipient, &m.body);
    if m.class != "test" {
        let dup = sqlx::query(
            "SELECT COUNT(*) AS n FROM msg_outbox
             WHERE body_hash = ? AND created_at > strftime('%Y-%m-%dT%H:%M:%SZ','now','-10 minutes')",
        )
        .bind(&hash)
        .fetch_one(pool)
        .await?
        .try_get::<i64, _>("n")?;
        if dup > 0 {
            return Ok(None);
        }
    }

    let id = crate::new_id();
    // is_failure is carried on the row because it decides whether this is the
    // news that breaks through quiet hours. It has a column of its own so that
    // a delivery error, written later to last_error, cannot overwrite it and
    // turn bad news into ordinary news held until morning.
    sqlx::query(
        "INSERT INTO msg_outbox (id, run_id, task_id, class, channel, recipient, recipient_label,
                                 subject, body, body_hash, state, is_failure, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'queued', ?, ?)",
    )
    .bind(&id)
    .bind(&m.run_id)
    .bind(&m.task_id)
    .bind(&m.class)
    .bind(&m.channel)
    .bind(&m.recipient)
    .bind(&m.recipient_label)
    .bind(&m.subject)
    .bind(&m.body)
    .bind(&hash)
    .bind(i64::from(m.is_failure))
    .bind(crate::now_iso())
    .execute(pool)
    .await?;
    Ok(Some(id))
}

pub async fn due_outbox(pool: &Pool, limit: i64) -> Result<Vec<OutboxRow>> {
    let rows = sqlx::query(
        "SELECT id, channel, class, recipient, subject, body, attempts, is_failure, last_error
         FROM msg_outbox
         WHERE state IN ('queued','retry_wait','deferred_quiet')
           AND (next_attempt_at IS NULL OR next_attempt_at <= ?)
         ORDER BY created_at LIMIT ?",
    )
    .bind(chrono::Utc::now().to_rfc3339())
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|r| {
            // The sentinel is still read as well as the column. Anything an
            // older daemon queued says so in last_error and nowhere else, and a
            // message already waiting must not change meaning because Errand
            // was updated while it sat there.
            let le: Option<String> = r.try_get("last_error")?;
            let legacy_failure = le.as_deref() == Some("failure-notice");
            Ok(OutboxRow {
                id: r.try_get("id")?,
                channel: r.try_get("channel")?,
                class: r.try_get("class")?,
                recipient: r.try_get("recipient")?,
                subject: r.try_get("subject")?,
                body: r.try_get("body")?,
                attempts: r.try_get("attempts")?,
                is_failure: r.try_get::<i64, _>("is_failure")? != 0 || legacy_failure,
            })
        })
        .collect()
}

pub async fn begin_send(pool: &Pool, id: &str) -> Result<()> {
    sqlx::query("UPDATE msg_outbox SET state = 'sending', attempts = attempts + 1 WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn mark_sent(pool: &Pool, id: &str, receipt: &str) -> Result<()> {
    sqlx::query(
        "UPDATE msg_outbox SET state = 'sent', sent_at = ?, provider_receipt = ? WHERE id = ?",
    )
    .bind(crate::now_iso())
    .bind(receipt)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn fail_outbox(
    pool: &Pool,
    id: &str,
    state: &str,
    error: &str,
    next_attempt_at: Option<String>,
) -> Result<()> {
    sqlx::query(
        "UPDATE msg_outbox SET state = ?, last_error = ?, next_attempt_at = ? WHERE id = ?",
    )
    .bind(state)
    .bind(error)
    .bind(next_attempt_at)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn defer_outbox(pool: &Pool, id: &str, until: String) -> Result<()> {
    sqlx::query("UPDATE msg_outbox SET state = 'deferred_quiet', next_attempt_at = ? WHERE id = ?")
        .bind(until)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// A send interrupted by a crash may or may not have arrived.
///
/// Retrying it blindly risks messaging someone twice; dropping it risks them
/// never hearing. Neither is acceptable silently, so it is marked uncertain and
/// shown as such.
pub async fn mark_sending_uncertain(pool: &Pool) -> Result<u64> {
    let res = sqlx::query(
        "UPDATE msg_outbox SET state = 'uncertain',
             last_error = 'Errand stopped while this was being sent, so it may or may not have arrived'
         WHERE state = 'sending'",
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

pub async fn outbox_for_run(pool: &Pool, run_id: &str) -> Result<Vec<(String, String, String)>> {
    let rows = sqlx::query(
        "SELECT channel, state, COALESCE(last_error,'') AS err FROM msg_outbox WHERE run_id = ?",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok((
                r.try_get("channel")?,
                r.try_get("state")?,
                r.try_get("err")?,
            ))
        })
        .collect()
}

// ---------------------------------------------------------------- webhooks --

#[derive(Debug, Clone)]
pub struct Webhook {
    pub id: String,
    pub url: String,
    pub events: Vec<String>,
    pub active: bool,
    pub failure_count: i64,
    pub last_error: Option<String>,
    pub created_at: String,
}

/// Only loopback and private addresses.
///
/// A webhook is a URL an outside client hands us and we then fetch on a
/// schedule, which is the shape of a request-forgery hole. Errand calls things
/// on your own machine or your own network, never the public internet.
pub fn webhook_target_allowed(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    if !matches!(u.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = u.host_str() else {
        return false;
    };
    if host == "localhost" || host.ends_with(".local") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => ip.is_loopback() || ip.is_private(),
        Ok(std::net::IpAddr::V6(ip)) => ip.is_loopback(),
        Err(_) => false,
    }
}

pub async fn create_webhook(
    pool: &Pool,
    token_id: &str,
    url: &str,
    events: &[String],
    secret_hash: &str,
) -> Result<String> {
    let id = crate::new_id();
    sqlx::query(
        "INSERT INTO webhooks (id, token_id, url, events, secret_hash, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(token_id)
    .bind(url)
    .bind(events.join(","))
    .bind(secret_hash)
    .bind(crate::now_iso())
    .execute(pool)
    .await?;
    Ok(id)
}

pub async fn list_webhooks(pool: &Pool) -> Result<Vec<Webhook>> {
    let rows = sqlx::query("SELECT * FROM webhooks ORDER BY created_at DESC")
        .fetch_all(pool)
        .await?;
    rows.into_iter()
        .map(|r| {
            let ev: String = r.try_get("events")?;
            Ok(Webhook {
                id: r.try_get("id")?,
                url: r.try_get("url")?,
                events: ev.split(',').map(str::to_string).collect(),
                active: r.try_get::<i64, _>("active")? != 0,
                failure_count: r.try_get("failure_count")?,
                last_error: r.try_get("last_error")?,
                created_at: r.try_get("created_at")?,
            })
        })
        .collect()
}

pub async fn delete_webhook(pool: &Pool, id: &str) -> Result<bool> {
    let res = sqlx::query("DELETE FROM webhooks WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Queue one delivery per subscriber interested in this event.
pub async fn fan_out_event(pool: &Pool, event: &str, payload: &serde_json::Value) -> Result<usize> {
    let hooks = list_webhooks(pool).await?;
    let mut n = 0;
    for h in hooks.into_iter().filter(|h| h.active) {
        if !h.events.iter().any(|e| e == event) {
            continue;
        }
        sqlx::query(
            "INSERT INTO webhook_deliveries (id, webhook_id, event, payload, created_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(crate::new_id())
        .bind(&h.id)
        .bind(event)
        .bind(payload.to_string())
        .bind(crate::now_iso())
        .execute(pool)
        .await?;
        n += 1;
    }
    Ok(n)
}

#[derive(Debug, Clone)]
pub struct Delivery {
    pub id: String,
    pub webhook_id: String,
    pub url: String,
    pub event: String,
    pub payload: String,
    pub attempts: i64,
}

pub async fn due_deliveries(pool: &Pool, limit: i64) -> Result<Vec<Delivery>> {
    let rows = sqlx::query(
        "SELECT d.id, d.webhook_id, d.event, d.payload, d.attempts, w.url
         FROM webhook_deliveries d JOIN webhooks w ON w.id = d.webhook_id
         WHERE d.delivered_at IS NULL AND w.active = 1
           AND (d.next_retry_at IS NULL OR d.next_retry_at <= ?)
         ORDER BY d.created_at LIMIT ?",
    )
    .bind(chrono::Utc::now().to_rfc3339())
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(Delivery {
                id: r.try_get("id")?,
                webhook_id: r.try_get("webhook_id")?,
                url: r.try_get("url")?,
                event: r.try_get("event")?,
                payload: r.try_get("payload")?,
                attempts: r.try_get("attempts")?,
            })
        })
        .collect()
}

pub async fn mark_delivered(pool: &Pool, id: &str, status: u16) -> Result<()> {
    sqlx::query(
        "UPDATE webhook_deliveries SET delivered_at = ?, status_code = ?, attempts = attempts + 1
         WHERE id = ?",
    )
    .bind(crate::now_iso())
    .bind(status as i64)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record a failed delivery, and disable a hook that has clearly gone away.
///
/// Retrying a dead endpoint forever is how a background worker quietly becomes
/// a load generator against somebody's machine.
pub async fn fail_delivery(
    pool: &Pool,
    id: &str,
    webhook_id: &str,
    error: &str,
    next_retry_at: Option<String>,
) -> Result<bool> {
    sqlx::query(
        "UPDATE webhook_deliveries SET attempts = attempts + 1, last_error = ?, next_retry_at = ?
         WHERE id = ?",
    )
    .bind(error)
    .bind(&next_retry_at)
    .bind(id)
    .execute(pool)
    .await?;

    if next_retry_at.is_none() {
        let n: i64 = sqlx::query(
            "UPDATE webhooks SET failure_count = failure_count + 1, last_error = ?
             WHERE id = ? RETURNING failure_count",
        )
        .bind(error)
        .bind(webhook_id)
        .fetch_one(pool)
        .await?
        .try_get("failure_count")?;

        if n >= 20 {
            sqlx::query("UPDATE webhooks SET active = 0 WHERE id = ?")
                .bind(webhook_id)
                .execute(pool)
                .await?;
            return Ok(true);
        }
    }
    Ok(false)
}

// ------------------------------------------------------------ idempotency --

/// A stored response for a repeated request.
pub async fn idempotent_replay(
    pool: &Pool,
    key: &str,
    endpoint: &str,
    request_hash: &str,
) -> Result<Option<std::result::Result<String, String>>> {
    let row = sqlx::query(
        "SELECT request_sha256, response_body FROM idempotency_keys
         WHERE key = ? AND endpoint = ?",
    )
    .bind(key)
    .bind(endpoint)
    .fetch_optional(pool)
    .await?;
    let Some(r) = row else { return Ok(None) };
    let stored: String = r.try_get("request_sha256")?;
    if stored != request_hash {
        // The same key with different content is a client bug, and replaying
        // the old answer would hide it.
        return Ok(Some(Err(
            "that idempotency key was already used for a different request".into(),
        )));
    }
    Ok(Some(Ok(r.try_get("response_body")?)))
}

pub async fn remember_idempotent(
    pool: &Pool,
    key: &str,
    endpoint: &str,
    request_hash: &str,
    status: u16,
    body: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT OR REPLACE INTO idempotency_keys
           (key, endpoint, request_sha256, response_status, response_body, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(key)
    .bind(endpoint)
    .bind(request_hash)
    .bind(status as i64)
    .bind(body)
    .bind(crate::now_iso())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_tokens(pool: &Pool) -> Result<Vec<(String, String, String, Option<String>)>> {
    let rows = sqlx::query(
        "SELECT id, name, scopes, last_used_at FROM api_tokens
         WHERE revoked_at IS NULL ORDER BY created_at",
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok((
                r.try_get("id")?,
                r.try_get("name")?,
                r.try_get("scopes")?,
                r.try_get("last_used_at")?,
            ))
        })
        .collect()
}

pub async fn revoke_token(pool: &Pool, id: &str) -> Result<bool> {
    let res =
        sqlx::query("UPDATE api_tokens SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL")
            .bind(crate::now_iso())
            .bind(id)
            .execute(pool)
            .await?;
    Ok(res.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_plan_a_run_wrote_becomes_the_way_the_job_is_done() {
        // Nobody is asked to read and approve a document before the task may
        // be used. It has just been watched doing the job, which is better
        // evidence than a reading of a plan.
        let pool = open_memory().await.unwrap();
        let task = a_task(&pool, serde_json::json!({"kind": "manual"})).await;
        let run = create_run(
            &pool,
            &task.id,
            "occ-1",
            "manual",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();

        let pb = crate::playbook::Playbook {
            version: next_playbook_version(&pool, &task.id).await.unwrap(),
            goal: "Book a court.".into(),
            sites: vec!["example.com".into()],
            preconditions: vec![],
            steps: vec![crate::playbook::Step {
                intent: "Open the grid.".into(),
                hint: None,
                decision: None,
            }],
            success: vec![],
            known_failures: vec![],
            never: vec![],
        };
        add_playbook_version(
            &pool,
            &task.id,
            &pb,
            crate::playbook::Source::Teach,
            Some(&run.id),
            None,
            false,
        )
        .await
        .unwrap();

        assert_eq!(
            adopt_plan_written_by(&pool, &task.id, &run.id)
                .await
                .unwrap(),
            Some(1)
        );
        assert_eq!(
            get_task(&pool, &task.id)
                .await
                .unwrap()
                .unwrap()
                .playbook_version,
            Some(1),
            "the task is not following the plan its own run wrote"
        );
    }

    #[tokio::test]
    async fn a_later_run_cannot_quietly_replace_a_plan_already_in_force() {
        // A playbook is distilled from pages written by strangers and handed
        // back to the agent as trusted instruction. Scrubbing removes secrets,
        // not instructions. One run against a changed page must not become
        // standing orders for every unattended run after it.
        let pool = open_memory().await.unwrap();
        let task = a_task(&pool, serde_json::json!({"kind": "manual"})).await;

        for (occ, adopted) in [("occ-1", Some(1)), ("occ-2", None)] {
            let run = create_run(
                &pool,
                &task.id,
                occ,
                "manual",
                crate::models::RunMode::NORMAL,
                None,
            )
            .await
            .unwrap();
            let pb = crate::playbook::Playbook {
                version: next_playbook_version(&pool, &task.id).await.unwrap(),
                goal: "Book a court.".into(),
                sites: vec![],
                preconditions: vec![],
                steps: vec![crate::playbook::Step {
                    intent: "Open the grid.".into(),
                    hint: None,
                    decision: None,
                }],
                success: vec![],
                known_failures: vec![],
                never: vec![],
            };
            add_playbook_version(
                &pool,
                &task.id,
                &pb,
                crate::playbook::Source::Refine,
                Some(&run.id),
                None,
                false,
            )
            .await
            .unwrap();
            assert_eq!(
                adopt_plan_written_by(&pool, &task.id, &run.id)
                    .await
                    .unwrap(),
                adopted,
                "at {occ}"
            );
        }
        // Still following the first, with the second waiting to be read.
        assert_eq!(
            get_task(&pool, &task.id)
                .await
                .unwrap()
                .unwrap()
                .playbook_version,
            Some(1)
        );
        assert_eq!(
            list_playbook_versions(&pool, &task.id).await.unwrap().len(),
            2
        );
    }

    #[tokio::test]
    async fn a_task_that_only_ever_fails_stops_running_itself() {
        // The loop nothing bounded. Every ceiling in this program bounds one
        // run: steps, minutes, money, heal cycles, turns. A task on an hourly
        // schedule whose site has changed fails inside all of them and then
        // does it again in an hour, for ever, each failure perfectly well
        // behaved.
        let pool = open_memory().await.unwrap();
        let task = a_task(&pool, serde_json::json!({"kind": "manual"})).await;
        set_task_status(&pool, &task.id, "ready").await.unwrap();

        for i in 0..crate::limits::FAILURES_BEFORE_PAUSING {
            let r = create_run(
                &pool,
                &task.id,
                &format!("occ-{i}"),
                "schedule",
                crate::models::RunMode::NORMAL,
                None,
            )
            .await
            .unwrap();
            finish_run_failed(
                &pool,
                &r.id,
                "target_unavailable",
                "The site is gone.",
                None,
            )
            .await
            .unwrap();
        }
        assert_eq!(
            consecutive_failures(&pool, &task.id).await.unwrap(),
            crate::limits::FAILURES_BEFORE_PAUSING
        );

        // A run the scheduler skipped is not the task working. It manufactures
        // those for a task that was still running or missed its window, so on a
        // short cron they land between the failures, and counting them would
        // reset the ceiling on exactly the tasks it exists for.
        let skipped = create_run(
            &pool,
            &task.id,
            "occ-skipped",
            "schedule",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();
        finish_run_skipped(&pool, &skipped.id, "it was still running", "")
            .await
            .unwrap();
        assert_eq!(
            consecutive_failures(&pool, &task.id).await.unwrap(),
            crate::limits::FAILURES_BEFORE_PAUSING,
            "a skipped run reset the count that stops a task failing for ever"
        );

        // One success clears the count outright rather than diluting it, or a
        // task that works four times out of five would creep up to the ceiling
        // and stop for no reason.
        let ok = create_run(
            &pool,
            &task.id,
            "occ-good",
            "schedule",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();
        finish_run_ok(&pool, &ok.id, "Done.", Some("Here it is."))
            .await
            .unwrap();
        assert_eq!(consecutive_failures(&pool, &task.id).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_pause_somebody_set_by_hand_is_never_undone_by_a_run_that_worked() {
        // Errand may undo its own guess. It may not overrule a person who
        // said stop.
        let pool = open_memory().await.unwrap();
        let task = a_task(&pool, serde_json::json!({"kind": "manual"})).await;
        // Only a task that runs itself can be stopped from running itself; a
        // draft was never going to.
        set_task_status(&pool, &task.id, "ready").await.unwrap();

        auto_pause_task(&pool, &task.id, "it kept failing")
            .await
            .unwrap();
        assert!(
            clear_auto_pause(&pool, &task.id).await.unwrap(),
            "its own pause should lift"
        );

        set_task_status(&pool, &task.id, "paused").await.unwrap();
        assert!(
            !clear_auto_pause(&pool, &task.id).await.unwrap(),
            "a person's pause was overruled by a successful run"
        );
        assert_eq!(
            get_task(&pool, &task.id).await.unwrap().unwrap().status,
            "paused"
        );
    }

    #[tokio::test]
    async fn migrations_apply_and_quick_check_passes() {
        let pool = open_memory().await.unwrap();
        assert_eq!(quick_check(&pool).await.unwrap(), "ok");
    }

    #[tokio::test]
    async fn task_and_run_roundtrip() {
        let pool = open_memory().await.unwrap();
        let t = create_task(
            &pool,
            NewTask {
                name: "Book tennis court".into(),
                description: "Book court 2 or 4 for Wednesday 19:00.".into(),
                emoji: Some("🎾".into()),
                schedule: serde_json::json!({"kind": "manual"}),
            },
        )
        .await
        .unwrap();
        assert_eq!(t.status, "draft");

        let run = create_run(
            &pool,
            &t.id,
            "occ-1",
            "manual",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();
        append_step(&pool, &run.id, "plan", "Loaded playbook", true, None)
            .await
            .unwrap();
        let seq = append_step(
            &pool,
            &run.id,
            "navigate",
            "Opened booking page",
            true,
            None,
        )
        .await
        .unwrap();
        assert_eq!(seq, 2);
        assert_eq!(list_steps(&pool, &run.id).await.unwrap().len(), 2);
        assert_eq!(count_busy_runs(&pool).await.unwrap(), 1);
    }

    /// One scheduled occurrence must only ever produce one run, even if the
    /// scheduler tries twice after a restart.
    #[tokio::test]
    async fn an_occurrence_cannot_run_twice() {
        let pool = open_memory().await.unwrap();
        let t = create_task(
            &pool,
            NewTask {
                name: "T".into(),
                description: "d".into(),
                emoji: None,
                schedule: serde_json::json!({"kind": "manual"}),
            },
        )
        .await
        .unwrap();
        create_run(
            &pool,
            &t.id,
            "2026-08-26T08:00",
            "schedule",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();
        let second = create_run(
            &pool,
            &t.id,
            "2026-08-26T08:00",
            "schedule",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await;
        assert!(second.is_err(), "duplicate occurrence must be rejected");
    }

    /// A run cannot be recorded as failed without answering the two questions
    /// the user actually has: what went wrong, and why.
    #[tokio::test]
    async fn a_failed_run_must_carry_an_explanation() {
        let pool = open_memory().await.unwrap();
        let t = create_task(
            &pool,
            NewTask {
                name: "T".into(),
                description: "d".into(),
                emoji: None,
                schedule: serde_json::json!({"kind": "manual"}),
            },
        )
        .await
        .unwrap();
        let run = create_run(
            &pool,
            &t.id,
            "occ",
            "manual",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();

        let bare = sqlx::query("UPDATE runs SET status = 'failed' WHERE id = ?")
            .bind(&run.id)
            .execute(&pool)
            .await;
        assert!(
            bare.is_err(),
            "failed run without explanation must be rejected"
        );

        let explained = sqlx::query(
            "UPDATE runs SET status = 'failed', failure_code = ?, failure_human = ? WHERE id = ?",
        )
        .bind("captcha_or_2fa_needed")
        .bind("The club site now asks for a code sent to your phone.")
        .bind(&run.id)
        .execute(&pool)
        .await;
        assert!(explained.is_ok());
    }

    #[tokio::test]
    async fn credentials_table_stores_no_secret() {
        let pool = open_memory().await.unwrap();
        let id = create_credential_meta(
            &pool,
            "Tennis club",
            "password",
            "example.com",
            Some("wolf"),
        )
        .await
        .unwrap();
        let all = list_credentials(&pool).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, id);
        // The wire type has no field capable of carrying a secret.
        let json = serde_json::to_string(&all[0]).unwrap();
        assert!(!json.contains("password_value"));
    }

    #[tokio::test]
    async fn the_fence_lets_one_action_through_per_occurrence() {
        let pool = open_memory().await.unwrap();
        let t = create_task(
            &pool,
            NewTask {
                name: "T".into(),
                description: "d".into(),
                emoji: None,
                schedule: serde_json::json!({"kind":"manual"}),
            },
        )
        .await
        .unwrap();
        let r = create_run(
            &pool,
            &t.id,
            "2026-08-26T08:00Z",
            "schedule",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();

        let v = arm_side_effect(&pool, &r.id, &t.id, "2026-08-26T08:00Z", "booking", "")
            .await
            .unwrap();
        let FenceVerdict::Armed(id) = v else {
            panic!("first arm should be allowed: {v:?}")
        };
        commit_side_effect(&pool, &id, r#"{"confirmation":"44821"}"#)
            .await
            .unwrap();

        // The same occurrence asking again gets told it is already done.
        let again = arm_side_effect(&pool, &r.id, &t.id, "2026-08-26T08:00Z", "booking", "")
            .await
            .unwrap();
        match again {
            FenceVerdict::AlreadyCommitted { evidence } => {
                assert!(evidence.unwrap().contains("44821"));
            }
            other => panic!("expected AlreadyCommitted, got {other:?}"),
        }
    }

    /// The failure the fence exists to prevent: a retry that picks a different
    /// resource must not slip past a guard keyed on the first choice.
    #[tokio::test]
    async fn a_retry_choosing_differently_cannot_double_book() {
        let pool = open_memory().await.unwrap();
        let t = create_task(
            &pool,
            NewTask {
                name: "Court".into(),
                description: "d".into(),
                emoji: None,
                schedule: serde_json::json!({"kind":"manual"}),
            },
        )
        .await
        .unwrap();
        let r1 = create_run(
            &pool,
            &t.id,
            "slot-A",
            "schedule",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();

        // First attempt books court 2.
        let FenceVerdict::Armed(id) =
            arm_side_effect(&pool, &r1.id, &t.id, "slot-A", "booking", "")
                .await
                .unwrap()
        else {
            panic!()
        };
        commit_side_effect(&pool, &id, r#"{"court":"2"}"#)
            .await
            .unwrap();

        // A retry of the same occurrence decides court 4 instead. Because the
        // key is the occurrence and not the court, it is still refused.
        let retry = arm_side_effect(&pool, &r1.id, &t.id, "slot-A", "booking", "")
            .await
            .unwrap();
        assert!(
            matches!(retry, FenceVerdict::AlreadyCommitted { .. }),
            "a different choice must not open a second booking: {retry:?}"
        );
    }

    #[tokio::test]
    async fn a_crash_between_arming_and_committing_demands_verification() {
        let pool = open_memory().await.unwrap();
        let t = create_task(
            &pool,
            NewTask {
                name: "T".into(),
                description: "d".into(),
                emoji: None,
                schedule: serde_json::json!({"kind":"manual"}),
            },
        )
        .await
        .unwrap();
        let r = create_run(
            &pool,
            &t.id,
            "slot-B",
            "schedule",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();

        // Armed, then the process died. Nobody knows if it went through.
        arm_side_effect(&pool, &r.id, &t.id, "slot-B", "booking", "")
            .await
            .unwrap();
        assert!(dangling_fences(&pool, &t.id, "slot-B").await.unwrap());

        let after = arm_side_effect(&pool, &r.id, &t.id, "slot-B", "booking", "")
            .await
            .unwrap();
        assert!(
            matches!(after, FenceVerdict::NeedsVerification { .. }),
            "an uncommitted fence must force a check before acting again: {after:?}"
        );
    }

    #[tokio::test]
    async fn aborting_frees_the_slot_for_a_later_attempt() {
        let pool = open_memory().await.unwrap();
        let t = create_task(
            &pool,
            NewTask {
                name: "T".into(),
                description: "d".into(),
                emoji: None,
                schedule: serde_json::json!({"kind":"manual"}),
            },
        )
        .await
        .unwrap();
        let r = create_run(
            &pool,
            &t.id,
            "slot-C",
            "schedule",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();

        let FenceVerdict::Armed(id) = arm_side_effect(&pool, &r.id, &t.id, "slot-C", "booking", "")
            .await
            .unwrap()
        else {
            panic!()
        };
        abort_side_effect(&pool, &id, "no free courts")
            .await
            .unwrap();
        assert!(!dangling_fences(&pool, &t.id, "slot-C").await.unwrap());

        let again = arm_side_effect(&pool, &r.id, &t.id, "slot-C", "booking", "")
            .await
            .unwrap();
        assert!(
            matches!(again, FenceVerdict::Armed(_)),
            "an aborted attempt should not block a real one: {again:?}"
        );
    }

    #[tokio::test]
    async fn different_occurrences_and_action_kinds_are_independent() {
        let pool = open_memory().await.unwrap();
        let t = create_task(
            &pool,
            NewTask {
                name: "T".into(),
                description: "d".into(),
                emoji: None,
                schedule: serde_json::json!({"kind":"manual"}),
            },
        )
        .await
        .unwrap();
        let r = create_run(
            &pool,
            &t.id,
            "w1",
            "schedule",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();

        let FenceVerdict::Armed(a) = arm_side_effect(&pool, &r.id, &t.id, "w1", "booking", "")
            .await
            .unwrap()
        else {
            panic!()
        };
        commit_side_effect(&pool, &a, "{}").await.unwrap();

        // Next week's slot is a different occurrence.
        assert!(matches!(
            arm_side_effect(&pool, &r.id, &t.id, "w2", "booking", "")
                .await
                .unwrap(),
            FenceVerdict::Armed(_)
        ));
        // Sending a message is a different kind of action.
        assert!(matches!(
            arm_side_effect(&pool, &r.id, &t.id, "w1", "message", "")
                .await
                .unwrap(),
            FenceVerdict::Armed(_)
        ));
    }

    /// The per-occurrence fence cannot see a manual re-trigger, because each
    /// manual run is its own slot. This is what notices it anyway.
    #[tokio::test]
    async fn a_repeat_of_the_same_action_within_minutes_is_visible() {
        let pool = open_memory().await.unwrap();
        let t = create_task(
            &pool,
            NewTask {
                name: "Book".into(),
                description: "d".into(),
                emoji: None,
                schedule: serde_json::json!({"kind":"manual"}),
            },
        )
        .await
        .unwrap();
        let r = create_run(
            &pool,
            &t.id,
            "manual/one",
            "manual",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();

        assert!(recent_commit(&pool, &t.id, "booking", "", 10)
            .await
            .unwrap()
            .is_none());

        let FenceVerdict::Armed(id) =
            arm_side_effect(&pool, &r.id, &t.id, "manual/one", "booking", "")
                .await
                .unwrap()
        else {
            panic!()
        };
        commit_side_effect(&pool, &id, r#"{"court":"2"}"#)
            .await
            .unwrap();

        // A second manual run is a different occurrence, so the fence allows it.
        let r2 = create_run(
            &pool,
            &t.id,
            "manual/two",
            "manual",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();
        assert!(matches!(
            arm_side_effect(&pool, &r2.id, &t.id, "manual/two", "booking", "")
                .await
                .unwrap(),
            FenceVerdict::Armed(_)
        ));
        // But the repeat is visible, which is what stops it happening silently.
        let recent = recent_commit(&pool, &t.id, "booking", "", 10)
            .await
            .unwrap();
        assert!(
            recent.is_some(),
            "a booking a moment ago must be visible to the next attempt"
        );
        assert_eq!(recent.unwrap().0, "manual/one");
    }

    #[tokio::test]
    async fn an_old_action_does_not_block_a_legitimate_new_one() {
        let pool = open_memory().await.unwrap();
        let t = create_task(
            &pool,
            NewTask {
                name: "Book".into(),
                description: "d".into(),
                emoji: None,
                schedule: serde_json::json!({"kind":"manual"}),
            },
        )
        .await
        .unwrap();
        let r = create_run(
            &pool,
            &t.id,
            "old",
            "manual",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();
        let FenceVerdict::Armed(id) = arm_side_effect(&pool, &r.id, &t.id, "old", "booking", "")
            .await
            .unwrap()
        else {
            panic!()
        };
        commit_side_effect(&pool, &id, "{}").await.unwrap();
        sqlx::query("UPDATE side_effects SET committed_at = '2020-01-01T00:00:00Z' WHERE id = ?")
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();

        // Booking again next week is normal and must not be obstructed.
        assert!(recent_commit(&pool, &t.id, "booking", "", 10)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn a_duplicate_occurrence_is_distinguished_from_a_real_fault() {
        let pool = open_memory().await.unwrap();
        let t = create_task(
            &pool,
            NewTask {
                name: "T".into(),
                description: "d".into(),
                emoji: None,
                schedule: serde_json::json!({"kind":"manual"}),
            },
        )
        .await
        .unwrap();
        try_create_run(
            &pool,
            &t.id,
            "slot",
            "schedule",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();
        let again = try_create_run(
            &pool,
            &t.id,
            "slot",
            "schedule",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await;
        assert!(
            matches!(again, Err(CreateRunError::AlreadyExists)),
            "a second run for one slot must be reported as a duplicate, not a fault"
        );

        // A genuine fault must NOT masquerade as a duplicate, or the occurrence
        // is lost silently.
        let bad = try_create_run(
            &pool,
            "no-such-task",
            "slot2",
            "schedule",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await;
        assert!(
            matches!(bad, Err(CreateRunError::Other(_))),
            "a foreign key failure must not be read as 'already ran'"
        );
    }

    #[tokio::test]
    async fn a_task_can_be_activated_from_draft() {
        let pool = open_memory().await.unwrap();
        let t = create_task(
            &pool,
            NewTask {
                name: "T".into(),
                description: "d".into(),
                emoji: None,
                schedule: serde_json::json!({"kind":"manual"}),
            },
        )
        .await
        .unwrap();
        assert_eq!(t.status, "draft");
        assert!(activate_task(&pool, &t.id).await.unwrap());
        assert_eq!(
            get_task(&pool, &t.id).await.unwrap().unwrap().status,
            "ready"
        );
    }

    #[tokio::test]
    async fn an_interrupted_run_is_closed_with_an_explanation_and_pauses_if_it_was_mid_action() {
        let pool = open_memory().await.unwrap();
        let t = create_task(
            &pool,
            NewTask {
                name: "T".into(),
                description: "d".into(),
                emoji: None,
                schedule: serde_json::json!({"kind":"manual"}),
            },
        )
        .await
        .unwrap();
        activate_task(&pool, &t.id).await.unwrap();
        let r = create_run(
            &pool,
            &t.id,
            "slot",
            "schedule",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();
        set_run_status(&pool, &r.id, "running").await.unwrap();
        arm_side_effect(&pool, &r.id, &t.id, "slot", "booking", "")
            .await
            .unwrap();

        let recovered = recover_interrupted_runs(&pool).await.unwrap();
        assert_eq!(recovered.len(), 1);
        assert!(
            recovered[0].2,
            "should be flagged as interrupted mid-action"
        );

        let run = get_run(&pool, &r.id).await.unwrap().unwrap();
        assert_eq!(run.status, "failed");
        let f = run.failure.unwrap();
        assert_eq!(f.code, "interrupted");
        assert!(f
            .plain_reason
            .contains("nobody knows whether it went through"));

        // The task is paused so it cannot quietly repeat the action.
        let task = get_task(&pool, &t.id).await.unwrap().unwrap();
        assert_eq!(task.status, "paused");
        assert!(task.auto_paused);
    }

    #[tokio::test]
    async fn a_busy_task_is_visible_so_a_second_run_can_be_refused() {
        let pool = open_memory().await.unwrap();
        let t = create_task(
            &pool,
            NewTask {
                name: "T".into(),
                description: "d".into(),
                emoji: None,
                schedule: serde_json::json!({"kind":"manual"}),
            },
        )
        .await
        .unwrap();
        assert!(busy_run_for_task(&pool, &t.id).await.unwrap().is_none());
        let r = create_run(
            &pool,
            &t.id,
            "s",
            "manual",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            busy_run_for_task(&pool, &t.id).await.unwrap(),
            Some(r.id.clone())
        );
        finish_run_ok(&pool, &r.id, "done", None).await.unwrap();
        assert!(busy_run_for_task(&pool, &t.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_stuck_task_can_be_released_by_the_user() {
        let pool = open_memory().await.unwrap();
        let t = create_task(
            &pool,
            NewTask {
                name: "T".into(),
                description: "d".into(),
                emoji: None,
                schedule: serde_json::json!({"kind":"manual"}),
            },
        )
        .await
        .unwrap();
        let r = create_run(
            &pool,
            &t.id,
            "slot",
            "schedule",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();
        arm_side_effect(&pool, &r.id, &t.id, "slot", "booking", "")
            .await
            .unwrap();
        assert!(dangling_fences(&pool, &t.id, "slot").await.unwrap());

        // "I checked, it did not happen."
        assert_eq!(
            clear_holds(&pool, &t.id, "user checked: not booked")
                .await
                .unwrap(),
            1
        );
        assert!(!dangling_fences(&pool, &t.id, "slot").await.unwrap());
        assert!(matches!(
            arm_side_effect(&pool, &r.id, &t.id, "slot", "booking", "")
                .await
                .unwrap(),
            FenceVerdict::Armed(_)
        ));
    }

    #[tokio::test]
    async fn confirming_a_hold_stops_the_slot_being_used_again() {
        let pool = open_memory().await.unwrap();
        let t = create_task(
            &pool,
            NewTask {
                name: "T".into(),
                description: "d".into(),
                emoji: None,
                schedule: serde_json::json!({"kind":"manual"}),
            },
        )
        .await
        .unwrap();
        let r = create_run(
            &pool,
            &t.id,
            "slot",
            "schedule",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();
        arm_side_effect(&pool, &r.id, &t.id, "slot", "booking", "")
            .await
            .unwrap();

        // "I checked, it did go through."
        assert_eq!(
            confirm_holds(&pool, &t.id, "user checked: booked")
                .await
                .unwrap(),
            1
        );
        assert!(matches!(
            arm_side_effect(&pool, &r.id, &t.id, "slot", "booking", "")
                .await
                .unwrap(),
            FenceVerdict::AlreadyCommitted { .. }
        ));
    }

    fn a_playbook() -> crate::playbook::Playbook {
        crate::playbook::Playbook {
            version: 1,
            goal: "Book a court.".into(),
            sites: vec!["example.com".into()],
            preconditions: vec![],
            steps: vec![crate::playbook::Step {
                intent: "Open the grid.".into(),
                hint: Some("/courts".into()),
                decision: None,
            }],
            success: vec![],
            known_failures: vec![],
            never: vec!["Never book two slots.".into()],
        }
    }

    #[tokio::test]
    async fn a_new_playbook_does_not_take_effect_until_it_is_approved() {
        let dir = std::env::temp_dir().join(format!("errand-pb-{}", crate::new_id()));
        std::env::set_var("ERRAND_DATA_DIR", &dir);
        let pool = open_memory().await.unwrap();
        let t = create_task(
            &pool,
            NewTask {
                name: "T".into(),
                description: "d".into(),
                emoji: None,
                schedule: serde_json::json!({"kind":"manual"}),
            },
        )
        .await
        .unwrap();

        let v = next_playbook_version(&pool, &t.id).await.unwrap();
        assert_eq!(v, 1);
        let mut pb = a_playbook();
        pb.version = v;
        add_playbook_version(
            &pool,
            &t.id,
            &pb,
            crate::playbook::Source::Teach,
            None,
            Some("first"),
            false,
        )
        .await
        .unwrap();

        // Written, listed, but not in force.
        assert_eq!(list_playbook_versions(&pool, &t.id).await.unwrap().len(), 1);
        assert!(
            active_playbook(&pool, &t.id).await.unwrap().is_none(),
            "an unapproved playbook must not be followed"
        );

        set_active_playbook(&pool, &t.id, v).await.unwrap();
        let active = active_playbook(&pool, &t.id).await.unwrap().unwrap();
        assert_eq!(active.goal, "Book a court.");
        std::fs::remove_dir_all(&dir).ok();
        std::env::remove_var("ERRAND_DATA_DIR");
    }

    #[tokio::test]
    async fn notes_come_back_oldest_first_so_the_newest_reads_last() {
        let pool = open_memory().await.unwrap();
        let t = create_task(
            &pool,
            NewTask {
                name: "T".into(),
                description: "d".into(),
                emoji: None,
                schedule: serde_json::json!({"kind":"manual"}),
            },
        )
        .await
        .unwrap();
        for (i, note) in ["older", "newer"].iter().enumerate() {
            let r = create_run(
                &pool,
                &t.id,
                &format!("s{i}"),
                "manual",
                crate::models::RunMode::NORMAL,
                None,
            )
            .await
            .unwrap();
            finish_run_ok(&pool, &r.id, "ok", None).await.unwrap();
            set_run_notes(&pool, &r.id, note).await.unwrap();
        }
        let notes = recent_notes(&pool, &t.id, 5).await.unwrap();
        assert_eq!(notes, vec!["older".to_string(), "newer".to_string()]);
    }

    #[tokio::test]
    async fn the_same_message_twice_in_a_minute_only_goes_once() {
        let pool = open_memory().await.unwrap();
        let m = || NewMessage {
            run_id: None,
            task_id: None,
            class: "notify".into(),
            channel: "telegram".into(),
            recipient: "123".into(),
            recipient_label: None,
            subject: None,
            body: "Court booked".into(),
            is_failure: false,
        };
        assert!(enqueue_message(&pool, m()).await.unwrap().is_some());
        assert!(
            enqueue_message(&pool, m()).await.unwrap().is_none(),
            "a duplicate is almost always a bug, and the person does not want it twice"
        );
    }

    #[tokio::test]
    async fn a_test_message_is_never_deduplicated() {
        let pool = open_memory().await.unwrap();
        let m = || NewMessage {
            run_id: None,
            task_id: None,
            class: "test".into(),
            channel: "telegram".into(),
            recipient: "123".into(),
            recipient_label: None,
            subject: None,
            body: "test".into(),
            is_failure: false,
        };
        assert!(enqueue_message(&pool, m()).await.unwrap().is_some());
        assert!(
            enqueue_message(&pool, m()).await.unwrap().is_some(),
            "pressing Send test twice must actually send twice"
        );
    }

    #[tokio::test]
    async fn a_queued_message_becomes_due_and_can_be_completed() {
        let pool = open_memory().await.unwrap();
        let id = enqueue_message(
            &pool,
            NewMessage {
                run_id: None,
                task_id: None,
                class: "notify".into(),
                channel: "telegram".into(),
                recipient: "123".into(),
                recipient_label: None,
                subject: None,
                body: "hello".into(),
                is_failure: true,
            },
        )
        .await
        .unwrap()
        .unwrap();

        let due = due_outbox(&pool, 10).await.unwrap();
        assert_eq!(due.len(), 1);
        assert!(
            due[0].is_failure,
            "bad news must be recognisable as bad news"
        );

        begin_send(&pool, &id).await.unwrap();
        mark_sent(&pool, &id, "42").await.unwrap();
        assert!(due_outbox(&pool, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_message_interrupted_mid_send_is_marked_uncertain_rather_than_resent() {
        let pool = open_memory().await.unwrap();
        let id = enqueue_message(
            &pool,
            NewMessage {
                run_id: None,
                task_id: None,
                class: "outreach".into(),
                channel: "whatsapp".into(),
                recipient: "15550100@c.us".into(),
                recipient_label: Some("Alex".into()),
                subject: None,
                body: "Court booked".into(),
                is_failure: false,
            },
        )
        .await
        .unwrap()
        .unwrap();
        begin_send(&pool, &id).await.unwrap();

        assert_eq!(mark_sending_uncertain(&pool).await.unwrap(), 1);
        // Not retried: messaging someone twice is worse than telling the truth
        // that nobody knows whether it arrived.
        assert!(due_outbox(&pool, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn something_needing_a_person_is_parked_rather_than_retried() {
        let pool = open_memory().await.unwrap();
        let id = enqueue_message(
            &pool,
            NewMessage {
                run_id: None,
                task_id: None,
                class: "notify".into(),
                channel: "whatsapp".into(),
                recipient: "x".into(),
                recipient_label: None,
                subject: None,
                body: "hi".into(),
                is_failure: false,
            },
        )
        .await
        .unwrap()
        .unwrap();
        fail_outbox(
            &pool,
            &id,
            "needs_user",
            "logged out; scan the QR code",
            None,
        )
        .await
        .unwrap();
        assert!(
            due_outbox(&pool, 10).await.unwrap().is_empty(),
            "retrying achieves nothing until a person acts"
        );
    }

    #[tokio::test]
    async fn news_that_a_task_failed_is_still_urgent_after_the_first_attempt_to_send_it_fails() {
        let pool = open_memory().await.unwrap();
        let id = enqueue_message(
            &pool,
            NewMessage {
                run_id: None,
                task_id: None,
                class: "notify".into(),
                channel: "telegram".into(),
                recipient: "123".into(),
                recipient_label: None,
                subject: None,
                body: "Your tennis booking did not go through.".into(),
                is_failure: true,
            },
        )
        .await
        .unwrap()
        .unwrap();

        begin_send(&pool, &id).await.unwrap();
        // Telegram was down. The real error goes where errors go.
        fail_outbox(
            &pool,
            &id,
            "retry_wait",
            "telegram answered 502",
            Some("2020-01-01T00:00:00Z".into()),
        )
        .await
        .unwrap();

        let due = due_outbox(&pool, 10).await.unwrap();
        assert_eq!(due.len(), 1);
        assert!(
            due[0].is_failure,
            "bad news must still be bad news on the second attempt, or quiet hours hold the one \
             message a person wanted waking up for"
        );
    }

    #[tokio::test]
    async fn bad_news_queued_by_an_older_version_is_still_recognised_after_an_update() {
        let pool = open_memory().await.unwrap();
        let id = enqueue_message(
            &pool,
            NewMessage {
                run_id: None,
                task_id: None,
                class: "notify".into(),
                channel: "telegram".into(),
                recipient: "123".into(),
                recipient_label: None,
                subject: None,
                body: "Your tennis booking did not go through.".into(),
                is_failure: false,
            },
        )
        .await
        .unwrap()
        .unwrap();
        // Exactly how an older daemon wrote it: the flag smuggled through the
        // error column and no flag of its own.
        sqlx::query(
            "UPDATE msg_outbox SET is_failure = 0, last_error = 'failure-notice' WHERE id = ?",
        )
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();

        let due = due_outbox(&pool, 10).await.unwrap();
        assert_eq!(due.len(), 1);
        assert!(
            due[0].is_failure,
            "a message already waiting must not change meaning because Errand was updated \
             while it sat there"
        );
    }

    #[test]
    fn a_webhook_may_only_point_at_your_own_machine_or_network() {
        // Errand fetches these on a schedule, so a public URL would make it a
        // request-forgery tool aimed wherever a client chose.
        assert!(webhook_target_allowed("http://127.0.0.1:3000/hook"));
        assert!(webhook_target_allowed("http://localhost:3000/hook"));
        assert!(webhook_target_allowed("http://192.168.1.50:8080/hook")); // scrub:allow private-ip testing that a LAN address is permitted
        assert!(webhook_target_allowed("http://kin.local/hook"));

        assert!(!webhook_target_allowed("https://example.com/hook"));
        assert!(!webhook_target_allowed(
            "http://169.254.169.254/latest/meta-data"
        ));
        assert!(!webhook_target_allowed("file:///etc/passwd"));
        assert!(!webhook_target_allowed("not a url"));
    }

    #[tokio::test]
    async fn a_retried_request_gets_the_same_answer_rather_than_a_second_booking() {
        let pool = open_memory().await.unwrap();
        let key = "kinai-msg-1";
        assert!(idempotent_replay(&pool, key, "/run", "hash-a")
            .await
            .unwrap()
            .is_none());

        remember_idempotent(&pool, key, "/run", "hash-a", 202, r#"{"id":"run-1"}"#)
            .await
            .unwrap();
        let replay = idempotent_replay(&pool, key, "/run", "hash-a")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(replay.unwrap(), r#"{"id":"run-1"}"#);
    }

    #[tokio::test]
    async fn the_same_key_with_different_content_is_refused_rather_than_replayed() {
        let pool = open_memory().await.unwrap();
        remember_idempotent(&pool, "k", "/run", "hash-a", 202, "{}")
            .await
            .unwrap();
        let r = idempotent_replay(&pool, "k", "/run", "hash-DIFFERENT")
            .await
            .unwrap()
            .unwrap();
        assert!(
            r.is_err(),
            "replaying the old answer would hide a client bug"
        );
    }

    #[tokio::test]
    async fn a_hook_that_keeps_failing_is_eventually_switched_off() {
        let pool = open_memory().await.unwrap();
        let tid = insert_token(&pool, "kinai", "h", "read,run,webhook")
            .await
            .unwrap();
        let wid = create_webhook(
            &pool,
            &tid,
            "http://127.0.0.1:9/x",
            &["run.finished".to_string()],
            "s",
        )
        .await
        .unwrap();

        let mut disabled = false;
        for _ in 0..20 {
            disabled = fail_delivery(&pool, "d", &wid, "refused", None)
                .await
                .unwrap();
        }
        assert!(
            disabled,
            "retrying a dead endpoint forever is a load generator"
        );
        assert!(!list_webhooks(&pool).await.unwrap()[0].active);
    }

    #[tokio::test]
    async fn events_only_reach_the_hooks_that_asked_for_them() {
        let pool = open_memory().await.unwrap();
        let tid = insert_token(&pool, "kinai", "h", "read,run,webhook")
            .await
            .unwrap();
        create_webhook(
            &pool,
            &tid,
            "http://127.0.0.1:3000/a",
            &["run.finished".to_string()],
            "s",
        )
        .await
        .unwrap();
        create_webhook(
            &pool,
            &tid,
            "http://127.0.0.1:3000/b",
            &["run.failed".to_string()],
            "s",
        )
        .await
        .unwrap();

        assert_eq!(
            fan_out_event(&pool, "run.finished", &serde_json::json!({}))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            fan_out_event(&pool, "task.updated", &serde_json::json!({}))
                .await
                .unwrap(),
            0
        );
    }

    // -------------------------------------------------- changing a task --

    async fn a_task(pool: &Pool, schedule: serde_json::Value) -> crate::models::Task {
        create_task(
            pool,
            NewTask {
                name: "Book tennis court".into(),
                description: "Book court 2 or 4 for Wednesday 19:00.".into(),
                emoji: Some("🎾".into()),
                schedule,
            },
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn changing_the_schedule_does_not_let_past_occurrences_run() {
        let pool = open_memory().await.unwrap();
        let t = a_task(&pool, serde_json::json!({"kind": "manual"})).await;

        // Switch to a cron that has notionally been due every morning for
        // years. Nothing before now may ever be treated as missed.
        let updated = update_task(
            &pool,
            &t.id,
            TaskPatch {
                schedule: Some(serde_json::json!({
                    "kind": "cron", "expr": "0 0 8 * * *", "tz": "UTC"
                })),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let floor = task_catch_up_floor(&pool, &t.id).await.unwrap().unwrap();
        assert!(
            floor >= t.created_at,
            "the floor must not sit before the task existed: {floor}"
        );

        // The promised next run is genuinely in the future, not this morning's
        // eight o'clock dressed up as a pending run. Parsed rather than
        // compared as text: next_run_at is written as an offset and now_iso as
        // a Z, and two formats compared as strings agree only by luck.
        let next = updated.next_run_at.expect("a cron task has a next run");
        let next: DateTime<Utc> = next.parse().unwrap();
        let floor_dt: DateTime<Utc> = floor.parse().unwrap();
        assert!(
            next > floor_dt,
            "the countdown promised a run at or below the floor: {next}"
        );
        assert!(
            next > Utc::now(),
            "the countdown promised a run that has already passed: {next}"
        );
    }

    #[tokio::test]
    async fn an_occurrence_already_recorded_stays_below_the_floor() {
        let pool = open_memory().await.unwrap();
        let t = a_task(&pool, serde_json::json!({"kind": "manual"})).await;

        // A slot this task has already used, and a manual run alongside it.
        // 'm' sorts above '2', so a bare MAX() would pick the manual id and the
        // floor would be gibberish.
        create_run(
            &pool,
            &t.id,
            "2099-01-01T08:00:00Z",
            "schedule",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();
        create_run(
            &pool,
            &t.id,
            &format!("manual/{}", crate::new_id()),
            "manual",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();

        update_task(
            &pool,
            &t.id,
            TaskPatch {
                schedule: Some(serde_json::json!({
                    "kind": "cron", "expr": "0 0 8 * * *", "tz": "UTC"
                })),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let floor = task_catch_up_floor(&pool, &t.id).await.unwrap().unwrap();
        assert_eq!(
            floor, "2099-01-01T08:00:00Z",
            "the floor must clear the last scheduled slot, and must not be a manual id"
        );
    }

    #[tokio::test]
    async fn a_task_turned_manual_stops_promising_a_next_run() {
        let pool = open_memory().await.unwrap();
        let t = a_task(
            &pool,
            serde_json::json!({"kind": "cron", "expr": "0 0 8 * * *", "tz": "UTC"}),
        )
        .await;

        let scheduled = update_task(
            &pool,
            &t.id,
            TaskPatch {
                schedule: Some(serde_json::json!({
                    "kind": "cron", "expr": "0 0 9 * * *", "tz": "UTC"
                })),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(scheduled.next_run_at.is_some());

        let manual = update_task(
            &pool,
            &t.id,
            TaskPatch {
                schedule: Some(serde_json::json!({"kind": "manual"})),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(
            manual.next_run_at.is_none(),
            "nothing else ever clears this, so a stale next run would show forever"
        );
    }

    #[tokio::test]
    async fn a_one_shot_whose_moment_has_gone_shows_no_next_run() {
        let pool = open_memory().await.unwrap();
        let t = a_task(&pool, serde_json::json!({"kind": "manual"})).await;

        let spent = update_task(
            &pool,
            &t.id,
            TaskPatch {
                schedule: Some(serde_json::json!({
                    "kind": "once", "at": "2020-01-01T08:00:00", "tz": "UTC"
                })),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(
            spent.next_run_at.is_none(),
            "a moment that has passed must not read as pending"
        );
    }

    #[tokio::test]
    async fn changing_the_schedule_does_not_wake_a_task_paused_for_checking() {
        let pool = open_memory().await.unwrap();
        let t = a_task(&pool, serde_json::json!({"kind": "manual"})).await;
        activate_task(&pool, &t.id).await.unwrap();
        auto_pause_task(&pool, &t.id, "needs_verification")
            .await
            .unwrap();

        let after = update_task(
            &pool,
            &t.id,
            TaskPatch {
                schedule: Some(serde_json::json!({
                    "kind": "cron", "expr": "0 0 8 * * *", "tz": "UTC"
                })),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(after.status, "paused");
        assert!(after.auto_paused);
        assert_eq!(after.paused_reason.as_deref(), Some("needs_verification"));
    }

    #[tokio::test]
    async fn editing_one_setting_leaves_the_others_alone() {
        let pool = open_memory().await.unwrap();
        let t = a_task(&pool, serde_json::json!({"kind": "manual"})).await;

        let after = update_task(
            &pool,
            &t.id,
            TaskPatch {
                name: Some("Book badminton court".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(after.name, "Book badminton court");
        assert_eq!(after.description, t.description, "description was blanked");
        assert_eq!(after.emoji, t.emoji);
        assert_eq!(after.schedule, t.schedule);
    }

    #[tokio::test]
    async fn changing_one_limit_leaves_the_ceilings_nobody_mentioned_where_they_were() {
        let pool = open_memory().await.unwrap();
        let t = a_task(&pool, serde_json::json!({"kind": "manual"})).await;

        // A task deliberately held to a single message and a long run.
        let set = update_task(
            &pool,
            &t.id,
            TaskPatch {
                limits: Some(serde_json::json!({
                    "max_steps": 200, "max_minutes": 45, "max_usd": 5.0,
                    "max_heal_cycles": 1, "max_messages": 1
                })),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(set.limits["max_messages"], 1);

        // Later, somebody edits only what a run may cost.
        let after = update_task(
            &pool,
            &t.id,
            TaskPatch {
                limits: Some(serde_json::json!({"max_usd": 2.0})),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(after.limits["max_usd"], 2.0);
        assert_eq!(
            after.limits["max_messages"], 1,
            "a task held to one message must not be let back up to three because somebody \
             adjusted its spending"
        );
        assert_eq!(after.limits["max_steps"], 200);
        assert_eq!(after.limits["max_minutes"], 45);
        assert_eq!(after.limits["max_heal_cycles"], 1);
    }

    #[tokio::test]
    async fn changing_one_notify_preference_leaves_the_rest_of_them_alone() {
        let pool = open_memory().await.unwrap();
        let t = a_task(&pool, serde_json::json!({"kind": "manual"})).await;

        let set = update_task(
            &pool,
            &t.id,
            TaskPatch {
                notify: Some(serde_json::json!({"on_success": false, "on_failure": true})),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(set.notify["on_failure"], true);

        let after = update_task(
            &pool,
            &t.id,
            TaskPatch {
                notify: Some(serde_json::json!({"on_success": true})),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(after.notify["on_success"], true);
        assert_eq!(
            after.notify["on_failure"], true,
            "asking to hear about successes must not stop the task telling anyone it failed"
        );
    }

    #[tokio::test]
    async fn taking_the_window_off_a_schedule_really_does_remove_it() {
        let pool = open_memory().await.unwrap();
        let t = a_task(
            &pool,
            serde_json::json!({
                "kind": "cron", "expr": "0 0 8 * * *", "tz": "UTC",
                "window": {"not_before": "08:00", "not_after": "09:00", "arm_early_s": 300}
            }),
        )
        .await;
        assert!(t.schedule.get("window").is_some());

        // Settings are merged; a schedule is not, and this is why. Somebody who
        // no longer wants their task confined to an hour of the morning has no
        // other way to say so.
        let after = update_task(
            &pool,
            &t.id,
            TaskPatch {
                schedule: Some(serde_json::json!({
                    "kind": "cron", "expr": "0 0 8 * * *", "tz": "UTC"
                })),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(
            after.schedule.get("window").is_none_or(|w| w.is_null()),
            "the window is still there: {}",
            after.schedule
        );
    }

    #[tokio::test]
    async fn a_task_that_is_not_coming_round_again_stops_showing_a_time_it_will_run() {
        let pool = open_memory().await.unwrap();
        let t = a_task(
            &pool,
            serde_json::json!({"kind": "once", "at": "2030-01-01T08:00:00", "tz": "UTC"}),
        )
        .await;

        set_next_run_at(&pool, &t.id, Some("2030-01-01T08:00:00+00:00"))
            .await
            .unwrap();
        assert!(get_task(&pool, &t.id)
            .await
            .unwrap()
            .unwrap()
            .next_run_at
            .is_some());

        // Its moment has been and gone. "Nothing next" has to be sayable, or
        // the task page goes on promising a run that already happened.
        set_next_run_at(&pool, &t.id, None).await.unwrap();
        assert_eq!(
            get_task(&pool, &t.id).await.unwrap().unwrap().next_run_at,
            None,
            "a one-off that has already run must not go on showing a run in the past"
        );
    }

    #[tokio::test]
    async fn the_sites_a_task_may_open_are_tidied_before_they_are_stored() {
        let pool = open_memory().await.unwrap();
        let t = a_task(&pool, serde_json::json!({"kind": "manual"})).await;

        let after = update_task(
            &pool,
            &t.id,
            TaskPatch {
                allowed_domains: Some(vec![
                    "  HTTPS://Tennis-Club.example/book?day=wed  ".into(),
                    "tennis-club.example".into(),
                ]),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(
            after.allowed_domains,
            serde_json::json!(["tennis-club.example"]),
            "what is stored must be exactly what the run-time check compares"
        );
    }

    #[tokio::test]
    async fn a_site_that_could_never_match_is_refused_and_nothing_is_saved() {
        let pool = open_memory().await.unwrap();
        let t = a_task(&pool, serde_json::json!({"kind": "manual"})).await;

        let err = update_task(
            &pool,
            &t.id,
            TaskPatch {
                name: Some("Renamed".into()),
                allowed_domains: Some(vec!["*.tennis-club.example".into()]),
                ..Default::default()
            },
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("tennis-club.example"), "unhelpful: {err}");

        let after = get_task(&pool, &t.id).await.unwrap().unwrap();
        assert_eq!(after.name, t.name, "the rename went through on a failure");
    }

    #[tokio::test]
    async fn an_archived_task_cannot_be_changed() {
        let pool = open_memory().await.unwrap();
        let t = a_task(&pool, serde_json::json!({"kind": "manual"})).await;
        set_task_status(&pool, &t.id, "archived").await.unwrap();

        let err = update_task(
            &pool,
            &t.id,
            TaskPatch {
                name: Some("Renamed".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("archived"), "unclear refusal: {err}");
    }

    // -------------------------------------------------------- fence scope --

    #[tokio::test]
    async fn an_empty_fence_scope_writes_exactly_the_key_it_always_did() {
        let pool = open_memory().await.unwrap();
        let t = a_task(&pool, serde_json::json!({"kind": "manual"})).await;
        let r = create_run(
            &pool,
            &t.id,
            "2026-08-26T08:00:00Z",
            "schedule",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();

        arm_side_effect(&pool, &r.id, &t.id, "2026-08-26T08:00:00Z", "booking", "")
            .await
            .unwrap();

        let key: String = sqlx::query("SELECT idempotency_key FROM side_effects WHERE task_id = ?")
            .bind(&t.id)
            .fetch_one(&pool)
            .await
            .unwrap()
            .try_get("idempotency_key")
            .unwrap();

        // Byte-identical to the pre-scope format. A key that changed shape
        // would stop matching a booking that has already been committed, and
        // the next attempt would read a taken slot as free.
        assert_eq!(key, format!("{}:2026-08-26T08:00:00Z:booking", t.id));
    }

    #[tokio::test]
    async fn a_scope_the_person_chose_gets_its_own_go_ahead() {
        let pool = open_memory().await.unwrap();
        let t = a_task(&pool, serde_json::json!({"kind": "manual"})).await;
        let r = create_run(
            &pool,
            &t.id,
            "slot",
            "schedule",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();

        // Telling two people about one booking is two separate promises, and
        // neither is a repeat of the other.
        let first = arm_side_effect(&pool, &r.id, &t.id, "slot", "message", "recipient-a")
            .await
            .unwrap();
        let second = arm_side_effect(&pool, &r.id, &t.id, "slot", "message", "recipient-b")
            .await
            .unwrap();
        assert!(matches!(first, FenceVerdict::Armed(_)));
        assert!(matches!(second, FenceVerdict::Armed(_)));

        // Asking twice for the same person is still refused.
        let FenceVerdict::Armed(id) = first else {
            unreachable!()
        };
        commit_side_effect(&pool, &id, "sent").await.unwrap();
        let repeat = arm_side_effect(&pool, &r.id, &t.id, "slot", "message", "recipient-a")
            .await
            .unwrap();
        assert!(matches!(repeat, FenceVerdict::AlreadyCommitted { .. }));
    }

    #[tokio::test]
    async fn a_repeat_check_for_one_person_ignores_what_was_sent_to_another() {
        let pool = open_memory().await.unwrap();
        let t = a_task(&pool, serde_json::json!({"kind": "manual"})).await;
        let r = create_run(
            &pool,
            &t.id,
            "slot",
            "schedule",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();

        let FenceVerdict::Armed(id) =
            arm_side_effect(&pool, &r.id, &t.id, "slot", "message", "recipient-a")
                .await
                .unwrap()
        else {
            unreachable!()
        };
        commit_side_effect(&pool, &id, "told Mum").await.unwrap();

        assert!(recent_commit(&pool, &t.id, "message", "recipient-a", 10)
            .await
            .unwrap()
            .is_some());
        assert!(
            recent_commit(&pool, &t.id, "message", "recipient-b", 10)
                .await
                .unwrap()
                .is_none(),
            "one person having been told must not silence the message to another"
        );
    }

    #[tokio::test]
    async fn a_message_to_one_person_still_counts_as_something_this_task_has_just_done() {
        let pool = open_memory().await.unwrap();
        let t = a_task(&pool, serde_json::json!({"kind": "manual"})).await;
        let r = create_run(
            &pool,
            &t.id,
            "slot",
            "schedule",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();

        let FenceVerdict::Armed(id) =
            arm_side_effect(&pool, &r.id, &t.id, "slot", "message", "recipient-mum")
                .await
                .unwrap()
        else {
            unreachable!()
        };
        commit_side_effect(&pool, &id, "told Mum the court was booked")
            .await
            .unwrap();

        // Every message is armed against the person it is for, so asking with
        // no scope finds nothing, which is how a guard meant to catch repeats
        // came to wave them all through.
        assert!(recent_commit(&pool, &t.id, "message", "", 10)
            .await
            .unwrap()
            .is_none());

        let (_, _, evidence, scope) = recent_commit_of_any_scope(&pool, &t.id, "message", 10)
            .await
            .unwrap()
            .expect("a message sent a moment ago is something this task has already done");
        assert_eq!(scope, "recipient-mum", "so the warning can name the person");
        assert_eq!(
            evidence.as_deref(),
            Some("told Mum the court was booked"),
            "the warning has to be able to say what was already done"
        );

        // Last week's message is not a repeat of anything.
        sqlx::query("UPDATE side_effects SET committed_at = '2020-01-01T00:00:00Z' WHERE id = ?")
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(recent_commit_of_any_scope(&pool, &t.id, "message", 10)
            .await
            .unwrap()
            .is_none());
    }

    // --------------------------------------------------------- recipients --

    #[tokio::test]
    async fn a_task_may_only_contact_the_people_it_was_given() {
        let pool = open_memory().await.unwrap();
        let mum = create_recipient(&pool, "Mum", "whatsapp", "+447700900112")
            .await
            .unwrap();
        let work = create_recipient(&pool, "Work", "apple_mail", "me@example.com")
            .await
            .unwrap();

        let tennis = a_task(&pool, serde_json::json!({"kind": "manual"})).await;
        let bills = a_task(&pool, serde_json::json!({"kind": "manual"})).await;

        link_recipient(&pool, &tennis.id, &mum, true, true)
            .await
            .unwrap();
        link_recipient(&pool, &bills.id, &work, false, true)
            .await
            .unwrap();

        let for_tennis = recipients_for_task(&pool, &tennis.id).await.unwrap();
        assert_eq!(for_tennis.len(), 1);
        assert_eq!(for_tennis[0].label, "Mum");

        let for_bills = recipients_for_task(&pool, &bills.id).await.unwrap();
        assert_eq!(for_bills.len(), 1);
        assert_eq!(for_bills[0].label, "Work");
        assert!(!for_bills[0].on_success, "the per-task flag was not kept");
        assert!(for_bills[0].on_failure);

        // Both exist globally; only the grant is per task.
        assert_eq!(list_recipients(&pool).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn taking_a_contact_off_one_task_leaves_the_others_alone() {
        let pool = open_memory().await.unwrap();
        let mum = create_recipient(&pool, "Mum", "whatsapp", "+447700900112")
            .await
            .unwrap();
        let a = a_task(&pool, serde_json::json!({"kind": "manual"})).await;
        let b = a_task(&pool, serde_json::json!({"kind": "manual"})).await;
        link_recipient(&pool, &a.id, &mum, true, true)
            .await
            .unwrap();
        link_recipient(&pool, &b.id, &mum, true, true)
            .await
            .unwrap();

        assert!(unlink_recipient(&pool, &a.id, &mum).await.unwrap());
        assert!(recipients_for_task(&pool, &a.id).await.unwrap().is_empty());
        assert_eq!(recipients_for_task(&pool, &b.id).await.unwrap().len(), 1);
        assert_eq!(
            list_recipients(&pool).await.unwrap().len(),
            1,
            "unlinking must not delete the person"
        );
    }

    #[tokio::test]
    async fn deleting_a_contact_takes_every_task_grant_with_it() {
        let pool = open_memory().await.unwrap();
        let mum = create_recipient(&pool, "Mum", "whatsapp", "+447700900112")
            .await
            .unwrap();
        let t = a_task(&pool, serde_json::json!({"kind": "manual"})).await;
        link_recipient(&pool, &t.id, &mum, true, true)
            .await
            .unwrap();

        assert!(delete_recipient(&pool, &mum).await.unwrap());
        assert!(
            recipients_for_task(&pool, &t.id).await.unwrap().is_empty(),
            "a task must not be left holding a grant to somebody who is gone"
        );
    }

    #[tokio::test]
    async fn changing_what_a_task_may_tell_someone_replaces_the_old_answer() {
        let pool = open_memory().await.unwrap();
        let mum = create_recipient(&pool, "Mum", "whatsapp", "+447700900112")
            .await
            .unwrap();
        let t = a_task(&pool, serde_json::json!({"kind": "manual"})).await;

        link_recipient(&pool, &t.id, &mum, true, true)
            .await
            .unwrap();
        link_recipient(&pool, &t.id, &mum, false, true)
            .await
            .unwrap();

        let granted = recipients_for_task(&pool, &t.id).await.unwrap();
        assert_eq!(granted.len(), 1, "relinking must not add a second row");
        assert!(!granted[0].on_success);
    }

    // ------------------------------------------------------- mail access --

    #[tokio::test]
    async fn a_task_cannot_touch_the_mail_until_somebody_says_it_may() {
        let pool = open_memory().await.unwrap();
        let t = a_task(&pool, serde_json::json!({"kind": "manual"})).await;
        assert!(
            mail_grant_for_task(&pool, &t.id).await.unwrap().is_none(),
            "a task must start with no reach into the mail at all"
        );

        grant_mail(&pool, &t.id, false).await.unwrap();
        let granted = mail_grant_for_task(&pool, &t.id).await.unwrap().unwrap();
        assert!(
            !granted.may_file,
            "being allowed to read the mail must not also allow rearranging it"
        );
    }

    #[tokio::test]
    async fn permission_to_read_the_mail_is_separate_from_permission_to_move_it() {
        let pool = open_memory().await.unwrap();
        let reader = a_task(&pool, serde_json::json!({"kind": "manual"})).await;
        let tidier = a_task(&pool, serde_json::json!({"kind": "manual"})).await;

        grant_mail(&pool, &reader.id, false).await.unwrap();
        grant_mail(&pool, &tidier.id, true).await.unwrap();

        assert!(
            !mail_grant_for_task(&pool, &reader.id)
                .await
                .unwrap()
                .unwrap()
                .may_file
        );
        assert!(
            mail_grant_for_task(&pool, &tidier.id)
                .await
                .unwrap()
                .unwrap()
                .may_file
        );
    }

    #[tokio::test]
    async fn granting_the_mail_twice_changes_the_answer_rather_than_adding_a_second_one() {
        let pool = open_memory().await.unwrap();
        let t = a_task(&pool, serde_json::json!({"kind": "manual"})).await;
        grant_mail(&pool, &t.id, true).await.unwrap();
        grant_mail(&pool, &t.id, false).await.unwrap();
        assert!(
            !mail_grant_for_task(&pool, &t.id)
                .await
                .unwrap()
                .unwrap()
                .may_file,
            "taking filing back away has to actually take it away"
        );
    }

    #[tokio::test]
    async fn taking_the_mail_grant_away_leaves_the_task_with_nothing() {
        let pool = open_memory().await.unwrap();
        let t = a_task(&pool, serde_json::json!({"kind": "manual"})).await;
        grant_mail(&pool, &t.id, true).await.unwrap();

        assert!(revoke_mail(&pool, &t.id).await.unwrap());
        assert!(mail_grant_for_task(&pool, &t.id).await.unwrap().is_none());
        assert!(
            !revoke_mail(&pool, &t.id).await.unwrap(),
            "revoking a grant that is not there is not a change"
        );
    }

    #[tokio::test]
    async fn a_way_of_sending_errand_does_not_have_is_refused_in_plain_words() {
        let pool = open_memory().await.unwrap();
        let err = create_recipient(&pool, "Mum", "carrier_pigeon", "+447700900112")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Telegram"), "no alternatives offered: {err}");
        assert!(list_recipients(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_agent_is_shown_who_it_is_writing_to_and_not_how_to_reach_them() {
        let pool = open_memory().await.unwrap();
        let mum = create_recipient(&pool, "Mum", "whatsapp", "+447700900112")
            .await
            .unwrap();
        let t = a_task(&pool, serde_json::json!({"kind": "manual"})).await;
        link_recipient(&pool, &t.id, &mum, true, true)
            .await
            .unwrap();

        let granted = &recipients_for_task(&pool, &t.id).await.unwrap()[0];
        assert_eq!(granted.address_masked, "+44 ••• ••12");
        assert!(
            !granted.address_masked.contains("7700900"),
            "the middle of the number survived the mask"
        );
    }

    #[test]
    fn a_masked_address_is_enough_to_recognise_and_not_enough_to_use() {
        assert_eq!(masked_address("whatsapp", "+447700900112"), "+44 ••• ••12");
        assert_eq!(masked_address("imessage", "+447700900112"), "+44 ••• ••12");
        assert_eq!(
            masked_address("apple_mail", "me@gmail.com"),
            "m•••@gmail.com"
        );
        assert_eq!(
            masked_address("imessage", "someone@example.com"),
            "s•••@example.com"
        );
        assert_eq!(masked_address("telegram", "@wolf"), "@w•••");
        assert_eq!(masked_address("telegram", "123456789"), "••• ••89");
        // Nothing useful to hide behind, so nothing is shown.
        assert_eq!(masked_address("whatsapp", "12"), "•••");
        assert_eq!(masked_address("apple_mail", "  "), "•••");
    }

    #[tokio::test]
    async fn an_artifact_is_recorded_and_found_by_id_and_linked_to_its_step() {
        let pool = open_memory().await.unwrap();
        let t = create_task(
            &pool,
            NewTask {
                name: "T".into(),
                description: "d".into(),
                emoji: None,
                schedule: serde_json::json!({"kind":"manual"}),
            },
        )
        .await
        .unwrap();
        let r = create_run(
            &pool,
            &t.id,
            "s",
            "manual",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();

        let artifact = record_artifact(&pool, &r.id, "screenshot", "runs/x/shots/y.png", 1234)
            .await
            .unwrap();
        let found = get_artifact(&pool, &artifact).await.unwrap().unwrap();
        assert_eq!(found.run_id, r.id);
        assert_eq!(found.rel_path, "runs/x/shots/y.png");
        assert!(found.masked);
        assert_eq!(found.bytes, Some(1234));
        // An id that was never issued is simply absent, never an error.
        assert!(get_artifact(&pool, "no-such-id").await.unwrap().is_none());

        let seq = append_step(&pool, &r.id, "screenshot", "the login page", true, None)
            .await
            .unwrap();
        attach_step_artifact(&pool, &r.id, seq, &artifact)
            .await
            .unwrap();
        let steps = list_steps(&pool, &r.id).await.unwrap();
        assert_eq!(steps[0].artifact_id.as_deref(), Some(artifact.as_str()));
    }

    #[tokio::test]
    async fn open_holds_are_counted_per_task_until_they_are_resolved() {
        let pool = open_memory().await.unwrap();
        let mk = |name: &str| {
            create_task(
                &pool,
                NewTask {
                    name: name.into(),
                    description: "d".into(),
                    emoji: None,
                    schedule: serde_json::json!({"kind":"manual"}),
                },
            )
        };
        let t = mk("T").await.unwrap();
        let other = mk("U").await.unwrap();
        let r = create_run(
            &pool,
            &t.id,
            "s",
            "manual",
            crate::models::RunMode::NORMAL,
            None,
        )
        .await
        .unwrap();

        assert_eq!(count_open_holds(&pool, &t.id).await.unwrap(), 0);
        assert!(open_hold_counts(&pool).await.unwrap().is_empty());

        arm_side_effect(&pool, &r.id, &t.id, "slot", "booking", "")
            .await
            .unwrap();
        assert_eq!(count_open_holds(&pool, &t.id).await.unwrap(), 1);
        assert_eq!(count_open_holds(&pool, &other.id).await.unwrap(), 0);
        let counts = open_hold_counts(&pool).await.unwrap();
        assert_eq!(counts.get(&t.id), Some(&1));
        // A task with nothing armed is absent rather than zero, so the list
        // page can tell "nothing to say" from "something to say".
        assert!(!counts.contains_key(&other.id));

        // "It did not happen" releases the fence; the count drops.
        clear_holds(&pool, &t.id, "checked: not booked")
            .await
            .unwrap();
        assert_eq!(count_open_holds(&pool, &t.id).await.unwrap(), 0);

        // So does "it already happened".
        arm_side_effect(&pool, &r.id, &t.id, "slot2", "booking", "")
            .await
            .unwrap();
        confirm_holds(&pool, &t.id, "checked: booked")
            .await
            .unwrap();
        assert_eq!(count_open_holds(&pool, &t.id).await.unwrap(), 0);
    }
}

// ------------------------------------------------------------------- ai --

/// Every place Errand could send a question, in a stable order so the Settings
/// list does not reshuffle itself between visits.
pub async fn list_providers(pool: &Pool) -> Result<Vec<crate::providers::Provider>> {
    let rows = sqlx::query(
        "SELECT id, kind, label, base_url, model, enabled, pinned, health_status, health_detail
           FROM provider_endpoints ORDER BY pinned DESC, label ASC",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| crate::providers::Provider {
            id: r.get("id"),
            kind: r.get("kind"),
            label: r.get("label"),
            base_url: r.get("base_url"),
            model: r.get("model"),
            enabled: r.get::<i64, _>("enabled") != 0,
            // `pinned` marks the ones Errand set up for itself, which is the same
            // question as "did a person type this in".
            discovered: r.get::<i64, _>("pinned") != 0,
            health: r.get("health_status"),
            health_detail: r.get("health_detail"),
        })
        .collect())
}

pub async fn upsert_provider(pool: &Pool, p: &crate::providers::Provider) -> Result<()> {
    sqlx::query(
        "INSERT INTO provider_endpoints
             (id, kind, label, base_url, model, enabled, pinned, health_status, health_detail)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
             kind = excluded.kind, label = excluded.label, base_url = excluded.base_url,
             model = excluded.model, enabled = excluded.enabled",
    )
    .bind(&p.id)
    .bind(&p.kind)
    .bind(&p.label)
    .bind(&p.base_url)
    .bind(&p.model)
    .bind(i64::from(p.enabled))
    .bind(i64::from(p.discovered))
    .bind(&p.health)
    .bind(&p.health_detail)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_provider_health(
    pool: &Pool,
    id: &str,
    status: &str,
    detail: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE provider_endpoints
            SET health_status = ?, health_detail = ?, checked_at = ? WHERE id = ?",
    )
    .bind(status)
    .bind(detail)
    .bind(crate::now_iso())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Remove a provider. Any role pointing at it falls back to the rest of the
/// chain rather than breaking, which is why the foreign key cascades.
pub async fn delete_provider(pool: &Pool, id: &str) -> Result<bool> {
    Ok(sqlx::query("DELETE FROM provider_endpoints WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected()
        > 0)
}

/// Which provider each role prefers, in preference order.
pub async fn list_role_bindings(pool: &Pool) -> Result<Vec<(crate::providers::Role, String)>> {
    let rows = sqlx::query(
        "SELECT role, endpoint_id FROM role_bindings WHERE scope = 'global' ORDER BY position ASC",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let role: String = r.get("role");
            crate::providers::Role::parse(&role)
                .map(|role| (role, r.get::<String, _>("endpoint_id")))
        })
        .collect())
}

/// Point a role at a provider. Passing None means "no preference", which puts
/// the role back on whatever is available rather than leaving it stuck.
pub async fn set_role_binding(
    pool: &Pool,
    role: crate::providers::Role,
    endpoint_id: Option<&str>,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM role_bindings WHERE role = ? AND scope = 'global'")
        .bind(role.as_str())
        .execute(&mut *tx)
        .await?;
    if let Some(id) = endpoint_id {
        sqlx::query(
            "INSERT INTO role_bindings (role, scope, position, endpoint_id)
             VALUES (?, 'global', 0, ?)",
        )
        .bind(role.as_str())
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod ai_tests {
    use super::*;
    use crate::providers::{Kind, Provider, Role};

    fn p(id: &str, kind: Kind, label: &str) -> Provider {
        Provider {
            id: id.into(),
            kind: kind.as_str().into(),
            label: label.into(),
            base_url: matches!(kind, Kind::OpenAiCompat)
                .then(|| "http://127.0.0.1:11434".to_string()),
            model: Some("some-model".into()),
            enabled: true,
            discovered: false,
            health: None,
            health_detail: None,
        }
    }

    #[tokio::test]
    async fn a_provider_survives_a_round_trip() {
        let pool = open_memory().await.unwrap();
        upsert_provider(&pool, &p("a", Kind::OpenAiCompat, "Ollama"))
            .await
            .unwrap();
        let back = list_providers(&pool).await.unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].label, "Ollama");
        assert_eq!(back[0].base_url.as_deref(), Some("http://127.0.0.1:11434"));
        assert!(back[0].enabled);
    }

    #[tokio::test]
    async fn switching_one_off_does_not_forget_it() {
        let pool = open_memory().await.unwrap();
        let mut prov = p("a", Kind::OpenAiCompat, "Ollama");
        upsert_provider(&pool, &prov).await.unwrap();
        prov.enabled = false;
        upsert_provider(&pool, &prov).await.unwrap();

        let back = list_providers(&pool).await.unwrap();
        assert_eq!(back.len(), 1, "it should still be listed, just off");
        assert!(!back[0].enabled);
    }

    #[tokio::test]
    async fn a_role_can_be_pointed_somewhere_and_then_released() {
        let pool = open_memory().await.unwrap();
        upsert_provider(&pool, &p("cli", Kind::ClaudeCli, "Claude"))
            .await
            .unwrap();
        upsert_provider(&pool, &p("loc", Kind::OpenAiCompat, "Ollama"))
            .await
            .unwrap();

        set_role_binding(&pool, Role::Fixer, Some("loc"))
            .await
            .unwrap();
        assert_eq!(
            list_role_bindings(&pool).await.unwrap(),
            vec![(Role::Fixer, "loc".to_string())]
        );

        set_role_binding(&pool, Role::Fixer, None).await.unwrap();
        assert!(list_role_bindings(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn deleting_a_provider_does_not_leave_a_role_pointing_at_a_ghost() {
        let pool = open_memory().await.unwrap();
        upsert_provider(&pool, &p("loc", Kind::OpenAiCompat, "Ollama"))
            .await
            .unwrap();
        set_role_binding(&pool, Role::Narrator, Some("loc"))
            .await
            .unwrap();

        assert!(delete_provider(&pool, "loc").await.unwrap());
        assert!(
            list_role_bindings(&pool).await.unwrap().is_empty(),
            "the binding must go with it, or the role resolves to nothing forever"
        );
    }

    #[tokio::test]
    async fn a_role_bound_to_something_that_cannot_do_the_job_still_resolves() {
        // The point of the chain: a person picking a model that turns out not to
        // be able to carry out a task must not silently stop every task from
        // running.
        //
        // A model nobody has asked is a different case, and used to be treated
        // as the same one: it goes first, because not having checked is not the
        // same as having found out it cannot.
        let pool = open_memory().await.unwrap();
        upsert_provider(&pool, &p("cli", Kind::ClaudeCli, "Claude"))
            .await
            .unwrap();
        upsert_provider(&pool, &p("loc", Kind::OpenAiCompat, "Ollama"))
            .await
            .unwrap();
        set_role_binding(&pool, Role::Executor, Some("loc"))
            .await
            .unwrap();

        let providers = list_providers(&pool).await.unwrap();
        let bindings = list_role_bindings(&pool).await.unwrap();

        let unchecked =
            crate::providers::resolve_chain(&providers, &bindings, Role::Executor, false);
        assert_eq!(
            unchecked.first().map(|p| p.id.as_str()),
            Some("loc"),
            "a model nobody has checked must still get the job it was chosen for"
        );

        let found_wanting =
            crate::providers::ToolsSeen::from([("loc".to_string(), crate::providers::Tools::No)]);
        let chain = crate::providers::resolve_chain_knowing(
            &providers,
            &bindings,
            Role::Executor,
            false,
            &found_wanting,
        );
        assert_eq!(chain.len(), 1);
        assert_eq!(
            chain[0].id, "cli",
            "it should fall through to the one that can"
        );
    }

    #[tokio::test]
    async fn health_is_recorded_where_the_interface_can_show_it() {
        let pool = open_memory().await.unwrap();
        upsert_provider(&pool, &p("loc", Kind::OpenAiCompat, "Ollama"))
            .await
            .unwrap();
        set_provider_health(&pool, "loc", "unreachable", Some("nothing at that address"))
            .await
            .unwrap();

        let back = list_providers(&pool).await.unwrap();
        assert_eq!(back[0].health.as_deref(), Some("unreachable"));
        assert_eq!(
            back[0].health_detail.as_deref(),
            Some("nothing at that address")
        );
    }
}

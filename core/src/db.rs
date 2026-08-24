//! Database access. The daemon owns this file exclusively.
//!
//! The UI never opens the database, not even read-only: a read-only connection
//! to a WAL database needs the shared-memory file and effectively a read-write
//! peer, so it fails exactly when the daemon is down and you are trying to work
//! out why. The UI goes through the API for everything.

use anyhow::{Context, Result};
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
        playbook_version: r.try_get("active_playbook_version")?,
        next_run_at: r.try_get("next_run_at")?,
        paused_reason: r.try_get("paused_reason")?,
        auto_paused: r.try_get::<i64, _>("auto_paused")? != 0,
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
    })
}

pub struct NewTask {
    pub name: String,
    pub description: String,
    pub emoji: Option<String>,
    pub schedule: serde_json::Value,
}

pub async fn create_task(pool: &Pool, t: NewTask) -> Result<crate::models::Task> {
    let id = crate::new_id();
    let now = crate::now_iso();
    sqlx::query(
        "INSERT INTO tasks (id, name, emoji, description_md, status, schedule_json,
                            created_at, updated_at)
         VALUES (?, ?, ?, ?, 'draft', ?, ?, ?)",
    )
    .bind(&id)
    .bind(&t.name)
    .bind(&t.emoji)
    .bind(&t.description)
    .bind(serde_json::to_string(&t.schedule)?)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    get_task(pool, &id)
        .await?
        .context("task vanished immediately after insert")
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

fn run_from_row(r: &sqlx::sqlite::SqliteRow) -> Result<crate::models::Run> {
    let code: Option<String> = r.try_get("failure_code")?;
    let human: Option<String> = r.try_get("failure_human")?;
    let failure = match (code, human) {
        (Some(c), Some(h)) => Some(crate::models::Failure {
            code: c,
            plain_reason: h,
            technical: r.try_get("failure_technical")?,
        }),
        _ => None,
    };
    Ok(crate::models::Run {
        id: r.try_get("id")?,
        task_id: r.try_get("task_id")?,
        occurrence_id: r.try_get("occurrence_id")?,
        mode: r.try_get("mode")?,
        trigger: r.try_get("trigger")?,
        triggered_by: r.try_get("triggered_by")?,
        status: r.try_get("status")?,
        scheduled_for: r.try_get("scheduled_for")?,
        started_at: r.try_get("started_at")?,
        finished_at: r.try_get("finished_at")?,
        summary: r.try_get("summary_md")?,
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
    mode: &str,
    triggered_by: Option<&str>,
) -> std::result::Result<crate::models::Run, CreateRunError> {
    let id = crate::new_id();
    let res = sqlx::query(
        "INSERT INTO runs (id, task_id, occurrence_id, mode, trigger, triggered_by, status, created_at)
         VALUES (?, ?, ?, ?, ?, ?, 'queued', ?)",
    )
    .bind(&id)
    .bind(task_id)
    .bind(occurrence_id)
    .bind(mode)
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
    mode: &str,
    triggered_by: Option<&str>,
) -> Result<crate::models::Run> {
    let id = crate::new_id();
    sqlx::query(
        "INSERT INTO runs (id, task_id, occurrence_id, mode, trigger, triggered_by, status, created_at)
         VALUES (?, ?, ?, ?, ?, ?, 'queued', ?)",
    )
    .bind(&id)
    .bind(task_id)
    .bind(occurrence_id)
    .bind(mode)
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

pub async fn finish_run_ok(pool: &Pool, run_id: &str, summary: &str) -> Result<()> {
    sqlx::query(
        "UPDATE runs SET status = 'succeeded', finished_at = ?, summary_md = ? WHERE id = ?",
    )
    .bind(crate::now_iso())
    .bind(summary)
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
    sqlx::query(
        "UPDATE runs SET status = 'failed', finished_at = ?, failure_code = ?,
                         failure_human = ?, failure_technical = ?
         WHERE id = ?",
    )
    .bind(crate::now_iso())
    .bind(code)
    .bind(human)
    .bind(technical)
    .bind(run_id)
    .execute(pool)
    .await?;
    Ok(())
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

/// Ask the fence for permission to do something irreversible.
///
/// The key is scoped to the occurrence, never to what the agent chose to do. A
/// key like `book:court2:19:00` would let a retry that picks court 4 straight
/// past the guard and double-book, which is the exact failure the fence exists
/// to prevent. One scheduled slot admits one irreversible action, whatever the
/// agent decides that action should be.
pub async fn arm_side_effect(
    pool: &Pool,
    run_id: &str,
    task_id: &str,
    occurrence_id: &str,
    action_kind: &str,
) -> Result<FenceVerdict> {
    let key = format!("{task_id}:{occurrence_id}:{action_kind}");
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
                                   idempotency_key, state, armed_at)
         VALUES (?, ?, ?, ?, ?, ?, 'armed', ?)
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

pub async fn set_next_run_at(pool: &Pool, task_id: &str, iso: &str) -> Result<()> {
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
pub async fn recent_commit(
    pool: &Pool,
    task_id: &str,
    action_kind: &str,
    within_minutes: i64,
) -> Result<Option<(String, String, Option<String>)>> {
    let row = sqlx::query(
        "SELECT occurrence_id, committed_at, evidence_json FROM side_effects
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
        ))),
        None => Ok(None),
    }
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
    sqlx::query(
        "INSERT INTO msg_outbox (id, run_id, task_id, class, channel, recipient, recipient_label,
                                 subject, body, body_hash, state, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'queued', ?)",
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
    .bind(crate::now_iso())
    .execute(pool)
    .await?;
    // Carried on the row so the outbox knows whether this is the news that
    // breaks through quiet hours.
    if m.is_failure {
        sqlx::query("UPDATE msg_outbox SET last_error = 'failure-notice' WHERE id = ?")
            .bind(&id)
            .execute(pool)
            .await?;
    }
    Ok(Some(id))
}

pub async fn due_outbox(pool: &Pool, limit: i64) -> Result<Vec<OutboxRow>> {
    let rows = sqlx::query(
        "SELECT id, channel, class, recipient, subject, body, attempts, last_error
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
            let le: Option<String> = r.try_get("last_error")?;
            Ok(OutboxRow {
                id: r.try_get("id")?,
                channel: r.try_get("channel")?,
                class: r.try_get("class")?,
                recipient: r.try_get("recipient")?,
                subject: r.try_get("subject")?,
                body: r.try_get("body")?,
                attempts: r.try_get("attempts")?,
                is_failure: le.as_deref() == Some("failure-notice"),
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

#[cfg(test)]
mod tests {
    use super::*;

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

        let run = create_run(&pool, &t.id, "occ-1", "manual", "normal", None)
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
        create_run(&pool, &t.id, "2026-08-26T08:00", "schedule", "normal", None)
            .await
            .unwrap();
        let second = create_run(&pool, &t.id, "2026-08-26T08:00", "schedule", "normal", None).await;
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
        let run = create_run(&pool, &t.id, "occ", "manual", "normal", None)
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
            "normal",
            None,
        )
        .await
        .unwrap();

        let v = arm_side_effect(&pool, &r.id, &t.id, "2026-08-26T08:00Z", "booking")
            .await
            .unwrap();
        let FenceVerdict::Armed(id) = v else {
            panic!("first arm should be allowed: {v:?}")
        };
        commit_side_effect(&pool, &id, r#"{"confirmation":"44821"}"#)
            .await
            .unwrap();

        // The same occurrence asking again gets told it is already done.
        let again = arm_side_effect(&pool, &r.id, &t.id, "2026-08-26T08:00Z", "booking")
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
        let r1 = create_run(&pool, &t.id, "slot-A", "schedule", "normal", None)
            .await
            .unwrap();

        // First attempt books court 2.
        let FenceVerdict::Armed(id) = arm_side_effect(&pool, &r1.id, &t.id, "slot-A", "booking")
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
        let retry = arm_side_effect(&pool, &r1.id, &t.id, "slot-A", "booking")
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
        let r = create_run(&pool, &t.id, "slot-B", "schedule", "normal", None)
            .await
            .unwrap();

        // Armed, then the process died. Nobody knows if it went through.
        arm_side_effect(&pool, &r.id, &t.id, "slot-B", "booking")
            .await
            .unwrap();
        assert!(dangling_fences(&pool, &t.id, "slot-B").await.unwrap());

        let after = arm_side_effect(&pool, &r.id, &t.id, "slot-B", "booking")
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
        let r = create_run(&pool, &t.id, "slot-C", "schedule", "normal", None)
            .await
            .unwrap();

        let FenceVerdict::Armed(id) = arm_side_effect(&pool, &r.id, &t.id, "slot-C", "booking")
            .await
            .unwrap()
        else {
            panic!()
        };
        abort_side_effect(&pool, &id, "no free courts")
            .await
            .unwrap();
        assert!(!dangling_fences(&pool, &t.id, "slot-C").await.unwrap());

        let again = arm_side_effect(&pool, &r.id, &t.id, "slot-C", "booking")
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
        let r = create_run(&pool, &t.id, "w1", "schedule", "normal", None)
            .await
            .unwrap();

        let FenceVerdict::Armed(a) = arm_side_effect(&pool, &r.id, &t.id, "w1", "booking")
            .await
            .unwrap()
        else {
            panic!()
        };
        commit_side_effect(&pool, &a, "{}").await.unwrap();

        // Next week's slot is a different occurrence.
        assert!(matches!(
            arm_side_effect(&pool, &r.id, &t.id, "w2", "booking")
                .await
                .unwrap(),
            FenceVerdict::Armed(_)
        ));
        // Sending a message is a different kind of action.
        assert!(matches!(
            arm_side_effect(&pool, &r.id, &t.id, "w1", "message")
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
        let r = create_run(&pool, &t.id, "manual/one", "manual", "normal", None)
            .await
            .unwrap();

        assert!(recent_commit(&pool, &t.id, "booking", 10)
            .await
            .unwrap()
            .is_none());

        let FenceVerdict::Armed(id) = arm_side_effect(&pool, &r.id, &t.id, "manual/one", "booking")
            .await
            .unwrap()
        else {
            panic!()
        };
        commit_side_effect(&pool, &id, r#"{"court":"2"}"#)
            .await
            .unwrap();

        // A second manual run is a different occurrence, so the fence allows it.
        let r2 = create_run(&pool, &t.id, "manual/two", "manual", "normal", None)
            .await
            .unwrap();
        assert!(matches!(
            arm_side_effect(&pool, &r2.id, &t.id, "manual/two", "booking")
                .await
                .unwrap(),
            FenceVerdict::Armed(_)
        ));
        // But the repeat is visible, which is what stops it happening silently.
        let recent = recent_commit(&pool, &t.id, "booking", 10).await.unwrap();
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
        let r = create_run(&pool, &t.id, "old", "manual", "normal", None)
            .await
            .unwrap();
        let FenceVerdict::Armed(id) = arm_side_effect(&pool, &r.id, &t.id, "old", "booking")
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
        assert!(recent_commit(&pool, &t.id, "booking", 10)
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
        try_create_run(&pool, &t.id, "slot", "schedule", "normal", None)
            .await
            .unwrap();
        let again = try_create_run(&pool, &t.id, "slot", "schedule", "normal", None).await;
        assert!(
            matches!(again, Err(CreateRunError::AlreadyExists)),
            "a second run for one slot must be reported as a duplicate, not a fault"
        );

        // A genuine fault must NOT masquerade as a duplicate, or the occurrence
        // is lost silently.
        let bad = try_create_run(&pool, "no-such-task", "slot2", "schedule", "normal", None).await;
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
        let r = create_run(&pool, &t.id, "slot", "schedule", "normal", None)
            .await
            .unwrap();
        set_run_status(&pool, &r.id, "running").await.unwrap();
        arm_side_effect(&pool, &r.id, &t.id, "slot", "booking")
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
        let r = create_run(&pool, &t.id, "s", "manual", "normal", None)
            .await
            .unwrap();
        assert_eq!(
            busy_run_for_task(&pool, &t.id).await.unwrap(),
            Some(r.id.clone())
        );
        finish_run_ok(&pool, &r.id, "done").await.unwrap();
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
        let r = create_run(&pool, &t.id, "slot", "schedule", "normal", None)
            .await
            .unwrap();
        arm_side_effect(&pool, &r.id, &t.id, "slot", "booking")
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
            arm_side_effect(&pool, &r.id, &t.id, "slot", "booking")
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
        let r = create_run(&pool, &t.id, "slot", "schedule", "normal", None)
            .await
            .unwrap();
        arm_side_effect(&pool, &r.id, &t.id, "slot", "booking")
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
            arm_side_effect(&pool, &r.id, &t.id, "slot", "booking")
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
            let r = create_run(&pool, &t.id, &format!("s{i}"), "manual", "normal", None)
                .await
                .unwrap();
            finish_run_ok(&pool, &r.id, "ok").await.unwrap();
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
}

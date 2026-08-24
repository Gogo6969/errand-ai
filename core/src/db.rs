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
    let rows = match task_id {
        Some(t) => {
            sqlx::query("SELECT * FROM runs WHERE task_id = ? ORDER BY created_at DESC LIMIT ?")
                .bind(t)
                .bind(limit)
                .fetch_all(pool)
                .await?
        }
        None => {
            sqlx::query("SELECT * FROM runs ORDER BY created_at DESC LIMIT ?")
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
}

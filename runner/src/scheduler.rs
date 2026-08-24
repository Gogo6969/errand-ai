//! The scheduler: what turns a task into a run without anyone asking.
//!
//! Not OS cron. The daemon computes occurrences itself, because the decisions
//! that matter here are ones cron cannot make: whether a run that came due
//! while the Mac was asleep should still happen, whether a window has closed,
//! and whether an occurrence already produced a run.
//!
//! The loop deliberately sleeps in short slices rather than one long sleep to
//! the next occurrence. A laptop suspends the whole process, and a timer set
//! for six hours away does not fire on time after four hours of sleep, so the
//! loop re-reads the clock instead of trusting an earlier calculation.

use chrono::{DateTime, Utc};
use errand_core::models::{Event, RunStatus};
use errand_core::schedule::{CatchUp, ScheduleSpec};

use crate::state::AppState;

/// How often to re-examine the schedule. Short enough that a wake from sleep is
/// noticed promptly, long enough to cost nothing.
const TICK: std::time::Duration = std::time::Duration::from_secs(20);

/// A clock jump larger than this means the machine slept, so the catch-up path
/// runs rather than the ordinary one.
const SLEEP_DETECT: i64 = 90;

/// Never replay more than this many missed occurrences at once, however long
/// the outage was.
const MAX_CATCH_UP: usize = 20;

/// How far ahead to look for occurrences whose run has to start early.
const MAX_ARM_EARLY_S: i64 = 15 * 60;

// An ordinary tick must never be mistaken for a wake from sleep, or every tick
// would take the catch-up path.
const _: () = assert!(SLEEP_DETECT > TICK.as_secs() as i64);

/// Where the last sweep got to, so a gap while the daemon was down is visible
/// on the next boot.
const LAST_TICK_KEY: &str = "scheduler.last_tick";

pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        // Resume from where the previous process stopped, not from now.
        // Seeding this with the current time would make every occurrence missed
        // during a restart, an update, or an overnight shutdown invisible,
        // which is precisely the gap the catch-up policy exists to decide about.
        let mut last_tick = match errand_core::db::get_setting(state.pool(), LAST_TICK_KEY).await {
            Ok(Some(v)) => v
                .as_str()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(Utc::now),
            _ => Utc::now(),
        };
        let downtime = (Utc::now() - last_tick).num_seconds();
        if downtime > SLEEP_DETECT {
            tracing::info!(
                downtime_s = downtime,
                "resuming after a gap; missed occurrences will follow each task's catch-up policy"
            );
        }
        // Close out anything the previous process left mid-flight. Without
        // this a killed run stays "running" forever: it never ends, never
        // explains itself, and the busy count is permanently wrong.
        match errand_core::db::recover_interrupted_runs(state.pool()).await {
            Ok(recovered) if !recovered.is_empty() => {
                let mid_action = recovered.iter().filter(|r| r.2).count();
                tracing::warn!(
                    count = recovered.len(),
                    mid_action,
                    "closed runs interrupted by a previous shutdown"
                );
                for (run_id, task_id, armed) in recovered {
                    state.emit(Event::RunFailed {
                        run_id,
                        task_id,
                        failure_code: errand_core::models::FailureCode::CrashDuringSideEffect,
                        failure_human: if armed {
                            "Errand stopped while this run was doing something that cannot be \
                             undone. Check the site before running it again."
                                .into()
                        } else {
                            "Errand stopped while this run was in progress.".into()
                        },
                    });
                }
            }
            Ok(_) => {}
            Err(e) => tracing::error!("could not recover interrupted runs: {e}"),
        }

        tracing::info!("scheduler started");
        loop {
            tokio::time::sleep(TICK).await;
            let now = Utc::now();
            let gap = (now - last_tick).num_seconds();
            // A gap is a gap: the machine slept, or the daemon was not running.
            // Both mean occurrences came due unobserved, and both are decided
            // by the task's catch-up policy rather than run blindly.
            let slept = gap > SLEEP_DETECT;
            if slept {
                tracing::info!(
                    gap_s = gap,
                    "gap since the last sweep; applying catch-up policy"
                );
            }
            if let Err(e) = tick(&state, now, last_tick, slept).await {
                tracing::error!("scheduler tick failed: {e}");
            }
            last_tick = now;
            // Persist after acting, never before: a crash mid-tick should make
            // the next boot reconsider that window rather than skip past it.
            if let Err(e) = errand_core::db::set_setting(
                state.pool(),
                LAST_TICK_KEY,
                &serde_json::Value::String(now.to_rfc3339()),
            )
            .await
            {
                tracing::warn!("could not record the scheduler position: {e}");
            }
        }
    });
}

/// One pass over every scheduled task.
async fn tick(
    state: &AppState,
    now: DateTime<Utc>,
    since: DateTime<Utc>,
    slept: bool,
) -> anyhow::Result<()> {
    if state.is_quiescing() {
        return Ok(());
    }
    let tasks = errand_core::db::list_tasks(state.pool(), false).await?;

    for task in tasks {
        // Paused keeps its computed next run visible but never enqueues, and
        // unpausing does not replay what it missed.
        if task.status != "ready" {
            continue;
        }
        let spec = match ScheduleSpec::from_json(&task.schedule) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(task = %task.id, "unreadable schedule: {e}");
                continue;
            }
        };
        if !spec.is_scheduled() {
            continue;
        }

        if let Err(e) = consider(state, &task, &spec, now, since, slept).await {
            tracing::error!(task = %task.id, "scheduling failed: {e}");
        }
    }
    Ok(())
}

async fn consider(
    state: &AppState,
    task: &errand_core::models::Task,
    spec: &ScheduleSpec,
    now: DateTime<Utc>,
    since: DateTime<Utc>,
    slept: bool,
) -> anyhow::Result<()> {
    // Look slightly into the future as well as the past, because a task with a
    // run window has to START before its occurrence in order to be logged in
    // and waiting when the barrier lifts.
    let horizon = now + chrono::Duration::seconds(MAX_ARM_EARLY_S);
    let (due, dropped) = spec.occurrences_between(since, horizon, MAX_CATCH_UP)?;

    if dropped > 0 {
        // Never let a truncated list look like a complete one.
        tracing::warn!(
            task = %task.id,
            dropped,
            "more occurrences were missed than can be replayed; the oldest were abandoned"
        );
    }

    // Anything whose start moment has not arrived yet waits for a later tick.
    let ready: Vec<DateTime<Utc>> = due
        .iter()
        .copied()
        .filter(|occ| now >= start_instant(task, spec, *occ))
        .collect();

    let to_run: Vec<DateTime<Utc>> = if ready.is_empty() {
        vec![]
    } else if slept || ready.len() > 1 {
        // Work out lateness from the single most recent occurrence rather than
        // from a possibly truncated list, so a long outage is judged correctly.
        let plan = match spec.catch_up {
            CatchUp::RunOnceLate => spec
                .last_occurrence_at_or_before(now)?
                .filter(|occ| ready.contains(occ))
                .map(|occ| spec.catch_up_plan(&[occ], now))
                .unwrap_or_default(),
            _ => spec.catch_up_plan(&ready, now),
        };
        // Record every occurrence the plan did not take, not only the case
        // where it took nothing. Under the default policy a five-occurrence
        // outage yields one run, and the other four would otherwise vanish
        // without a trace, which is exactly the silent gap this is meant to
        // prevent.
        for missed in &ready {
            if !plan.contains(missed) {
                record_skip(state, task, spec, *missed, "missed_while_asleep").await?;
            }
        }
        plan
    } else {
        ready
    };

    for occurrence in to_run {
        if spec.window_missed(occurrence, now)? {
            record_skip(state, task, spec, occurrence, "missed_window").await?;
            continue;
        }
        // One run per task at a time. A task whose runs take longer than its
        // interval would otherwise pile up agents on the same site, each with
        // its own browser and its own idea of what has been done.
        if let Some(busy) = errand_core::db::busy_run_for_task(state.pool(), &task.id).await? {
            tracing::info!(task = %task.id, %busy, "still running; skipping this occurrence");
            record_skip(state, task, spec, occurrence, "still_running").await?;
            continue;
        }
        fire(state, task, spec, occurrence).await?;
    }

    // The countdown must show the moment the run will actually begin, which is
    // the same expression the firing decision uses.
    if let Some(next) = spec.next_after(now)? {
        let shown = start_instant(task, spec, next);
        errand_core::db::set_next_run_at(state.pool(), &task.id, &shown.to_rfc3339()).await?;
    }
    Ok(())
}

/// When a run for this occurrence should actually begin: early enough to be
/// logged in before a window opens, plus this task's stable jitter.
///
/// One expression, used both to decide when to fire and to display the
/// countdown, so the time shown is the time it happens.
fn start_instant(
    task: &errand_core::models::Task,
    spec: &ScheduleSpec,
    occurrence: DateTime<Utc>,
) -> DateTime<Utc> {
    spec.start_at(occurrence)
        + errand_core::schedule::jitter_for(&task.id, occurrence, spec.jitter_s)
}

async fn fire(
    state: &AppState,
    task: &errand_core::models::Task,
    spec: &ScheduleSpec,
    occurrence: DateTime<Utc>,
) -> anyhow::Result<()> {
    let occurrence_id = spec.occurrence_id(occurrence);

    // A run that began something irreversible and never reported back leaves
    // nobody knowing whether it happened. Firing again could duplicate it, so
    // the occurrence stops here, says so, and pauses the task for a human.
    if errand_core::db::dangling_fences(state.pool(), &task.id, &occurrence_id).await? {
        tracing::warn!(task = %task.id, occurrence = %occurrence_id,
            "occurrence has an unresolved action; refusing to re-fire");
        record_skip(state, task, spec, occurrence, "needs_verification").await?;
        errand_core::db::auto_pause_task(state.pool(), &task.id, "needs_verification").await?;
        return Ok(());
    }

    let run = match errand_core::db::try_create_run(
        state.pool(),
        &task.id,
        &occurrence_id,
        "schedule",
        "normal",
        None,
    )
    .await
    {
        Ok(r) => r,
        // Expected: this slot already ran.
        Err(errand_core::db::CreateRunError::AlreadyExists) => return Ok(()),
        // Anything else must surface. Treating a database fault as "already
        // ran" would drop the occurrence with no run, no failure and no record.
        Err(errand_core::db::CreateRunError::Other(e)) => {
            return Err(e.context("creating a scheduled run"))
        }
    };

    errand_core::db::append_step(
        state.pool(),
        &run.id,
        "plan",
        &format!("Scheduled run for {occurrence_id}"),
        true,
        None,
    )
    .await?;

    state.emit(Event::RunStatus {
        run_id: run.id.clone(),
        task_id: task.id.clone(),
        status: RunStatus::Queued,
    });

    tracing::info!(task = %task.name, run = %run.id, occurrence = %occurrence_id, "firing");
    tokio::spawn(crate::executor::run_to_completion(
        state.clone(),
        run.id.clone(),
    ));
    Ok(())
}

/// Record an occurrence that deliberately did not run, so the history shows the
/// decision instead of a gap.
async fn record_skip(
    state: &AppState,
    task: &errand_core::models::Task,
    spec: &ScheduleSpec,
    occurrence: DateTime<Utc>,
    code: &str,
) -> anyhow::Result<()> {
    let occurrence_id = spec.occurrence_id(occurrence);
    let run = match errand_core::db::try_create_run(
        state.pool(),
        &task.id,
        &occurrence_id,
        "schedule",
        "normal",
        None,
    )
    .await
    {
        Ok(r) => r,
        Err(errand_core::db::CreateRunError::AlreadyExists) => return Ok(()),
        // A fault here would turn a skip that was meant to leave a record into
        // exactly the silent gap this function exists to prevent.
        Err(errand_core::db::CreateRunError::Other(e)) => {
            return Err(e.context("recording a skipped occurrence"))
        }
    };

    let human = match code {
        "needs_verification" => concat!(
            "An earlier attempt at this slot began something that cannot be undone and never ",
            "confirmed whether it finished, so nobody knows if it went through. Rather than ",
            "risk doing it twice, this run was not started and the task has been paused. ",
            "Check the site, then resume the task."
        )
        .to_string(),
        "still_running" => concat!(
            "This run came due while the previous one was still going. Two runs of the same task ",
            "at once would each act without knowing what the other had done, so this one was not ",
            "started. If this keeps happening, the task is taking longer than the gap between its ",
            "runs."
        )
        .to_string(),
        "missed_window" => format!(
            "This run had to happen inside a set time window, and by the time it came \
             round the window had closed. Nothing was attempted, because doing it late \
             would have been worse than not doing it. Next run: {}.",
            spec.next_after(Utc::now())
                .ok()
                .flatten()
                .map(|d| d.to_rfc3339())
                .unwrap_or_else(|| "none scheduled".into())
        ),
        _ if spec.catch_up == CatchUp::Skip => format!(
            "This run came due while the computer was asleep or Errand was not running. \
             This task is set to skip anything it misses rather than run it late, so \
             nothing was attempted. It was scheduled for {occurrence_id}."
        ),
        _ => format!(
            "This run came due while the computer was asleep or Errand was not running, \
             and by the time it woke up more than {} minutes had passed, which is longer \
             than this task allows for running late. It was scheduled for {occurrence_id}.",
            spec.catch_up_grace_min
        ),
    };

    errand_core::db::finish_run_skipped(state.pool(), &run.id, code, &human).await?;
    state.emit(Event::RunFinished {
        run_id: run.id,
        task_id: task.id.clone(),
        status: RunStatus::Skipped,
        summary: Some(human),
    });
    Ok(())
}

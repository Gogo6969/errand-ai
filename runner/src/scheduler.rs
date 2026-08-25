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
    // Never look back past the moment this task's schedule started being true.
    //
    // The sweep cursor is global: one row, shared by every task. So the moment
    // somebody changes one task's schedule, every occurrence of the NEW
    // schedule between that shared cursor and now reads as missed. Switching a
    // task to a daily cron would fire a historical run on the spot and burn
    // that occurrence id for good, because a burnt id can never run again.
    //
    // `occurrences_between` advances with `next_after`, which is strictly
    // after, so an occurrence landing exactly on the floor second is excluded.
    // That is the safe direction: the floor is the instant the old schedule
    // stopped applying, and nothing before it was ever missed.
    let since = catch_up_floor(state, task, since, now).await?;

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

/// The sweep cursor, raised to this task's catch-up floor.
///
/// Returns whichever is later. A task with no floor has been running against
/// its current schedule all along and keeps the shared cursor unchanged.
async fn catch_up_floor(
    state: &AppState,
    task: &errand_core::models::Task,
    since: DateTime<Utc>,
    now: DateTime<Utc>,
) -> anyhow::Result<DateTime<Utc>> {
    let Some(stored) = errand_core::db::task_catch_up_floor(state.pool(), &task.id).await? else {
        return Ok(since);
    };
    let Ok(floor) = stored.parse::<DateTime<Utc>>() else {
        // A floor nobody can read is not a reason to replay a week of history.
        // Standing still until now means no catch-up for this task on this
        // sweep; occurrences still to come are unaffected.
        tracing::warn!(
            task = %task.id,
            floor = %stored,
            "cannot read this task's catch-up floor, so nothing missed will be made up for it"
        );
        return Ok(now.max(since));
    };
    Ok(since.max(floor))
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
        // This slot already produced a run, so it must not produce a second.
        // Correct, but not silent: the decision to fire was made and then
        // dropped, and a line here is the only trace that the occurrence was
        // ever considered.
        Err(errand_core::db::CreateRunError::AlreadyExists) => {
            tracing::warn!(
                task = %task.id,
                name = %task.name,
                occurrence = %occurrence_id,
                "this occurrence already has a run, so it was not started again"
            );
            return Ok(());
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::testkit::{self, a_ready_manual_task};
    use serde_json::json;

    /// Three in the morning, every morning. Chosen so that a sweep pretending
    /// to have missed a week has seven perfectly good occurrences to replay if
    /// nothing stops it.
    const EVERY_MORNING: &str = "0 0 3 * * *";

    /// Every minute, for the test that needs occurrences to fall due inside a
    /// jump of a few minutes rather than waiting for an hour to come round.
    const EVERY_MINUTE: &str = "0 * * * * *";

    async fn put_on_schedule(api: &testkit::Api, id: &str, expr: &str, catch_up: &str) {
        let (code, body) = api
            .patch(
                &format!("/v1/tasks/{id}"),
                json!({ "schedule": {
                    "kind": "cron", "expr": expr, "tz": "UTC", "catch_up": catch_up
                }}),
            )
            .await;
        assert_eq!(code, 200, "putting it on a schedule failed: {body}");
    }

    #[tokio::test]
    async fn putting_a_task_on_a_schedule_does_not_run_it_for_slots_it_never_missed() {
        // The failure this guards against: the sweep cursor is one row shared
        // by every task, so the moment a task is moved onto a repeating
        // schedule, every occurrence of the NEW schedule between that shared
        // cursor and now looks missed. The task fires on the spot for a morning
        // that has already gone, and burns that occurrence id for good, because
        // a slot that has run can never run again.
        let api = testkit::start().await;
        let id = a_ready_manual_task(&api).await;
        put_on_schedule(&api, &id, EVERY_MORNING, "run_once_late").await;

        // A sweep that believes it has not looked since last week, which is
        // what a laptop that has been shut for a few days looks like.
        let now = Utc::now();
        tick(&api.state, now, now - chrono::Duration::days(7), true)
            .await
            .expect("the sweep ran");

        let runs = api.get(&format!("/v1/runs?task_id={id}")).await;
        let items = runs["items"].as_array().expect("a list of runs");
        assert!(
            items.is_empty(),
            "a task moved onto a schedule produced {} run(s) for slots it was never around for: \
             {runs}",
            items.len()
        );

        // And it still promises a next run, so nothing has quietly stopped.
        let task = api.get(&format!("/v1/tasks/{id}")).await;
        assert!(
            task["next_run_at"].is_string(),
            "the task must still say when it runs next: {task}"
        );
    }

    #[tokio::test]
    async fn the_floor_does_not_blind_a_task_to_the_slots_that_come_after_it() {
        // The other half of the same rule. Without this, "never replay history"
        // would be indistinguishable from "never notice anything", and a task
        // that came due while the Mac was asleep would silently never happen
        // with nothing in its history to say so.
        let api = testkit::start().await;
        let id = a_ready_manual_task(&api).await;
        put_on_schedule(&api, &id, EVERY_MINUTE, "skip").await;

        // Three minutes on. Occurrences above the floor have now come due; this
        // task is set to skip rather than run late, so each one is recorded as
        // skipped instead of being run, which is the decision being visible
        // rather than a gap in the history.
        let later = Utc::now() + chrono::Duration::minutes(3);
        tick(&api.state, later, later - chrono::Duration::days(7), true)
            .await
            .expect("the sweep ran");

        let runs = api.get(&format!("/v1/runs?task_id={id}")).await;
        let items = runs["items"].as_array().expect("a list of runs");
        assert!(
            !items.is_empty(),
            "an occurrence after the floor must still be noticed: {runs}"
        );
        assert!(
            items.iter().all(|r| r["status"] == "skipped"),
            "this task skips what it misses, so nothing should have been run: {runs}"
        );
    }
}

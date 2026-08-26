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
///
/// This is a cap on the PAST. The lookahead below is counted separately, and
/// must stay that way: when both shared one budget, a schedule that comes round
/// every few seconds filled the whole allowance with occurrences that had not
/// happened yet, evicted every missed one, and so ran nothing at all.
const MAX_CATCH_UP: usize = 20;

/// How far ahead to look for occurrences whose run has to start early.
///
/// The same hour a window's `arm_early_s` is allowed to reach, because a task
/// told to be logged in an hour beforehand would otherwise never be looked at
/// until fifteen minutes before, and would arrive after the barrier lifted
/// instead of waiting behind it.
const MAX_ARM_EARLY_S: i64 = 60 * 60;

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
        // unpausing does not replay what it missed. That promise is why any
        // half-drained catch-up is forgotten here: a task paused with make-up
        // runs still owed must come back to life facing forwards, not with a
        // queue of mornings that have already gone.
        if task.status != "ready" {
            if let Err(e) = forget_backlog(state, &task.id).await {
                tracing::warn!(task = %task.id, "could not clear the catch-up backlog: {e}");
            }
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
    let floor = occurrence_floor(state, task, now).await?;

    // A make-up run that could not start last sweep is still owed. The shared
    // cursor always moves on, so without this the rest of a catch-up backlog
    // would fall behind it and never be looked at again.
    let owed_from = backlog_cursor(state, &task.id).await?;

    // Two bounds, in two different spaces, and they are not interchangeable.
    //
    // `scan_from` bounds OCCURRENCES, because that is what `occurrences_between`
    // walks. It reaches back past the cursor by the largest jitter this task can
    // add, since an occurrence from just before the last sweep may only now have
    // reached its jittered start.
    //
    // `lower` bounds START INSTANTS, because that is what the decision to fire
    // actually turns on. Firing is `lower < start <= now`: half-open, so an
    // instant belongs to exactly one sweep and cannot be both fired and skipped.
    let widen = chrono::Duration::seconds(spec.jitter_s.max(0));
    let mut scan_from = since - widen;
    let mut lower = since;
    if let Some(owed) = owed_from {
        scan_from = scan_from.min(owed);
        lower = lower.min(spec.start_at(owed));
    }
    if let Some(f) = floor {
        // The floor is a floor on the occurrence, never on the shifted bound,
        // or jitter would let an occurrence from before a schedule change back
        // in through the widening.
        scan_from = scan_from.max(f);
        // An occurrence just above a fresh floor can have a start instant
        // behind the cursor, because a window arms early. Nothing above the
        // floor has ever been considered, so the start bound gives way to it.
        if f > lower {
            lower = spec.start_at(f);
        }
    }

    let (mut due, dropped) = spec.occurrences_between(scan_from, now, MAX_CATCH_UP)?;

    if dropped > 0 {
        // Never let a truncated list look like a complete one.
        tracing::warn!(
            task = %task.id,
            dropped,
            "more occurrences were missed than can be replayed; the oldest were abandoned"
        );
    }

    // Look slightly into the future as well as the past, because a task with a
    // run window has to START before its occurrence in order to be logged in
    // and waiting when the barrier lifts. One occurrence is enough: a later one
    // starts a whole period further on, so if this one's moment to begin has
    // not arrived, neither has any other's. Counted outside MAX_CATCH_UP on
    // purpose: the cap is there to bound a replay of the past, and sharing it
    // with the lookahead is what let the future crowd the past out entirely.
    if let Some(next) = spec.next_after(now)? {
        if next <= now + chrono::Duration::seconds(MAX_ARM_EARLY_S) {
            due.push(next);
        }
    }

    // Select on the moment the run would begin, not on the occurrence. A task
    // with jitter starts after its occurrence, so choosing by occurrence let a
    // run whose jittered start landed past the end of the sweep fall between two
    // sweeps and never happen at all.
    let started: Vec<DateTime<Utc>> = due
        .iter()
        .copied()
        .filter(|occ| {
            let s = start_instant(task, spec, *occ);
            s > lower && now >= s
        })
        .collect();

    // An occurrence still ahead of us, whose run merely has to begin early, is
    // not a late one. Keeping the two apart matters: the catch-up policy decides
    // what to do about lateness, and an early start judged by it would be
    // written off as missed while the computer was asleep.
    let (ready, early): (Vec<DateTime<Utc>>, Vec<DateTime<Utc>>) =
        started.into_iter().partition(|occ| *occ <= now);

    // Whether these occurrences are being made up for rather than simply coming
    // round. It decides both what the catch-up policy is asked and, further
    // down, what happens when one of them meets a run already in progress.
    let late = slept || ready.len() > 1 || owed_from.is_some();
    let replaying = late && spec.catch_up == CatchUp::RunAll;

    // Carried alongside each occurrence, because the lookahead entries appended
    // afterwards are early rather than late and must not be treated as make-ups.
    let mut to_run: Vec<(DateTime<Utc>, bool)> = if ready.is_empty() {
        vec![]
    } else if late {
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
        plan.into_iter().map(|occ| (occ, true)).collect()
    } else {
        ready.into_iter().map(|occ| (occ, false)).collect()
    };

    to_run.extend(early.into_iter().map(|occ| (occ, false)));

    // The first occurrence this sweep could not get to, if any.
    let mut owed: Option<DateTime<Utc>> = None;
    let queued = to_run.len();
    for (position, (occurrence, making_up)) in to_run.into_iter().enumerate() {
        if spec.window_missed(occurrence, now)? {
            record_skip(state, task, spec, occurrence, "missed_window").await?;
            continue;
        }
        // One run per task at a time. A task whose runs take longer than its
        // interval would otherwise pile up agents on the same site, each with
        // its own browser and its own idea of what has been done.
        if let Some(busy) = errand_core::db::busy_run_for_task(state.pool(), &task.id).await? {
            if replaying && making_up {
                // A run that is being made up for has not had its turn yet, so
                // it must not be written off as skipped. Recording a skip would
                // spend the occurrence id, and a spent id can never run: asking
                // for every missed run to be made up would deliver the first and
                // silently destroy the rest. It waits instead, and the sweep
                // that finds the task free picks up where this one stopped.
                tracing::info!(
                    task = %task.id,
                    %busy,
                    occurrence = %spec.occurrence_id(occurrence),
                    waiting = queued - position,
                    "a run being made up for is waiting for the one in progress; it keeps its place"
                );
                owed = Some(occurrence);
                break;
            }
            tracing::info!(task = %task.id, %busy, "still running; skipping this occurrence");
            // A make-up that cannot be queued is not the task over-running, and
            // must not be described as though it were: during the outage it did
            // not run at all.
            let code = if making_up {
                "catch_up_collision"
            } else {
                "still_running"
            };
            record_skip(state, task, spec, occurrence, code).await?;
            continue;
        }
        fire(state, task, spec, occurrence).await?;
    }

    // Remember an unfinished replay, and forget one that has been drained. A
    // second before the occurrence, because the scan that re-derives it starts
    // strictly after this instant and the occurrence itself must survive it.
    let backlog = owed.map(|o| o - chrono::Duration::seconds(1));
    if backlog != owed_from {
        set_backlog_cursor(state, &task.id, backlog).await?;
    }

    // The countdown must show the moment the run will actually begin, which is
    // the same expression the firing decision uses. Written every sweep, even
    // when there is nothing left to come: a one-off that has been and gone has
    // to clear its promise, or the task page goes on counting down to a moment
    // in the past for as long as the task exists.
    let shown = spec
        .next_after(now)?
        .map(|next| start_instant(task, spec, next).to_rfc3339());
    errand_core::db::set_next_run_at(state.pool(), &task.id, shown.as_deref()).await?;
    Ok(())
}

/// The earliest occurrence this task may still act on.
///
/// `None` means no floor: the task has been running against its current
/// schedule all along, so the shared sweep cursor is the only bound.
async fn occurrence_floor(
    state: &AppState,
    task: &errand_core::models::Task,
    now: DateTime<Utc>,
) -> anyhow::Result<Option<DateTime<Utc>>> {
    let Some(stored) = errand_core::db::task_catch_up_floor(state.pool(), &task.id).await? else {
        return Ok(None);
    };
    let Ok(floor) = stored.parse::<DateTime<Utc>>() else {
        // A floor nobody can read is not a reason to replay a week of history.
        // Standing still at now means no catch-up for this task on this sweep;
        // occurrences still to come are unaffected.
        tracing::warn!(
            task = %task.id,
            floor = %stored,
            "cannot read this task's catch-up floor, so nothing missed will be made up for it"
        );
        return Ok(Some(now));
    };
    Ok(Some(floor))
}

/// The settings row holding one task's unfinished catch-up.
fn backlog_key(task_id: &str) -> String {
    format!("scheduler.backlog.{task_id}")
}

/// The instant a catch-up replay for this task stopped at, if one is unfinished.
///
/// Occurrences after it are still owed. It exists because the sweep cursor is
/// shared by every task and always moves on, so a make-up run that had to wait
/// for the run in progress would otherwise be left behind it, unspent and
/// unreachable: a run promised, never made, and never explained.
async fn backlog_cursor(state: &AppState, task_id: &str) -> anyhow::Result<Option<DateTime<Utc>>> {
    let Some(v) = errand_core::db::get_setting(state.pool(), &backlog_key(task_id)).await? else {
        return Ok(None);
    };
    Ok(v.as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc)))
}

async fn set_backlog_cursor(
    state: &AppState,
    task_id: &str,
    at: Option<DateTime<Utc>>,
) -> anyhow::Result<()> {
    let value = match at {
        Some(d) => serde_json::Value::String(d.to_rfc3339()),
        None => serde_json::Value::Null,
    };
    errand_core::db::set_setting(state.pool(), &backlog_key(task_id), &value).await
}

/// Drop any unfinished catch-up for this task, writing only if there was one.
async fn forget_backlog(state: &AppState, task_id: &str) -> anyhow::Result<()> {
    if backlog_cursor(state, task_id).await?.is_some() {
        set_backlog_cursor(state, task_id, None).await?;
    }
    Ok(())
}

/// When a run for this occurrence should actually begin: early enough to be
/// logged in before a window opens, plus this task's stable jitter.
///
/// One expression, used by every writer of the time a task will next run: the
/// sweep, the activate route, and `update_task` in core, so the time shown is
/// the time it happens. Three separate versions of it is how the countdown came
/// to jump the moment the scheduler first ticked.
pub(crate) fn start_instant(
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
        errand_core::models::RunMode::NORMAL,
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
        errand_core::models::RunMode::NORMAL,
        None,
    )
    .await
    {
        Ok(r) => r,
        // The slot already has a run, so its history is not a blank, but the
        // reason this sweep wanted to skip it is about to be thrown away, and
        // that reason is the only thing that could explain a decision nobody
        // else recorded. It goes to the log rather than nowhere.
        Err(errand_core::db::CreateRunError::AlreadyExists) => {
            tracing::info!(
                task = %task.id,
                name = %task.name,
                occurrence = %occurrence_id,
                reason = code,
                "this occurrence already has a run, so the reason it was skipped was not recorded"
            );
            return Ok(());
        }
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
        "catch_up_collision" => concat!(
            "This one was missed while the computer was asleep or Errand was not running, and by ",
            "the time it could have been made up for, another run of the same task was already ",
            "going. Two at once would each act without knowing what the other had done, so this ",
            "one was not started. Nothing about it went wrong; it simply lost its turn."
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

    /// The runs the scheduler itself started, which is what these tests are
    /// about. A task cannot be put on a schedule until it has really done the
    /// job once, so every scheduled task in here also has one manual run
    /// standing as that proof, and counting it would make every assertion here
    /// off by one.
    async fn scheduled_runs(
        api: &crate::api::testkit::Api,
        task_id: &str,
    ) -> Vec<serde_json::Value> {
        let runs = api.get(&format!("/v1/runs?task_id={task_id}")).await;
        runs["items"]
            .as_array()
            .expect("a list of runs")
            .iter()
            .filter(|r| r["trigger"] == "schedule")
            .cloned()
            .collect()
    }
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

    /// Twice a minute: fast enough that a quarter of an hour of lookahead holds
    /// far more occurrences than the catch-up cap, which is what used to leave
    /// this schedule running nothing at all.
    const EVERY_HALF_MINUTE: &str = "*/30 * * * * *";

    async fn put_on_schedule(api: &testkit::Api, id: &str, expr: &str, catch_up: &str) {
        put_on_schedule_with(
            api,
            id,
            json!({
                "kind": "cron", "expr": expr, "tz": "UTC", "catch_up": catch_up
            }),
        )
        .await;
    }

    async fn put_on_schedule_with(api: &testkit::Api, id: &str, schedule: serde_json::Value) {
        let (code, body) = api
            .patch(&format!("/v1/tasks/{id}"), json!({ "schedule": schedule }))
            .await;
        assert_eq!(code, 200, "putting it on a schedule failed: {body}");
    }

    /// A run already under way, the way pressing "Run now" leaves one.
    ///
    /// Every test below keeps one of these in flight while it sweeps, so an
    /// occurrence that is noticed leaves a record instead of starting a real
    /// agent on the machine running the suite.
    async fn a_run_in_flight(api: &testkit::Api, task_id: &str) -> String {
        errand_core::db::try_create_run(
            &api.pool,
            task_id,
            &format!("manual/{}", errand_core::new_id()),
            "manual",
            errand_core::models::RunMode::NORMAL,
            None,
        )
        .await
        .expect("a run")
        .id
    }

    /// The occurrences this task has a row for, scheduled ones only.
    async fn slots_recorded(api: &testkit::Api, task_id: &str) -> Vec<String> {
        let runs = api.get(&format!("/v1/runs?task_id={task_id}")).await;
        runs["items"]
            .as_array()
            .expect("a list of runs")
            .iter()
            // Only the slots the scheduler filled. A task cannot be put on a
            // schedule until it has really done the job once, so every task
            // here also carries one manual run standing as that proof.
            .filter(|r| r["trigger"] == "schedule")
            .filter_map(|r| r["occurrence_id"].as_str())
            .filter(|o| !o.starts_with("manual/"))
            .map(|o| o.to_string())
            .collect()
    }

    /// Keep a real agent out of the test suite.
    ///
    /// A sweep that fires spawns the executor, and the executor prefers
    /// CLAUDE_BIN over anything installed. Pointing it at a script that does
    /// nothing means a test can let a run actually start, which is the only way
    /// to check that a missed run eventually happens, without launching an
    /// agent on somebody's machine.
    fn no_real_agent() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let stub =
                std::env::temp_dir().join(format!("errand-no-agent-{}", errand_core::new_id()));
            std::fs::write(&stub, "#!/bin/sh\nexit 0\n").expect("writing the stand-in agent");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
                    .expect("making the stand-in agent runnable");
            }
            std::env::set_var("CLAUDE_BIN", &stub);
        });
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

        let items = scheduled_runs(&api, &id).await;
        let runs = json!({ "items": items.clone() });
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

        let items = scheduled_runs(&api, &id).await;
        let runs = json!({ "items": items.clone() });
        assert!(
            !items.is_empty(),
            "an occurrence after the floor must still be noticed: {runs}"
        );
        assert!(
            items.iter().all(|r| r["status"] == "skipped"),
            "this task skips what it misses, so nothing should have been run: {runs}"
        );
    }

    #[tokio::test]
    async fn a_task_that_comes_round_every_half_minute_is_not_ignored_for_ever() {
        // The failure this guards against: the sweep looked back at what was
        // missed and forward at what has to start early using ONE budget of
        // twenty occurrences, and kept the newest twenty. A schedule this fast
        // filled that budget entirely with occurrences that had not happened
        // yet, so every slot that had come due was thrown out of the list before
        // anything looked at it. The task sat there for ever, doing nothing,
        // with nothing in its history to show for it.
        let api = testkit::start().await;
        let id = a_ready_manual_task(&api).await;
        put_on_schedule(&api, &id, EVERY_HALF_MINUTE, "run_once_late").await;
        let _busy = a_run_in_flight(&api, &id).await;

        let later = Utc::now() + chrono::Duration::seconds(90);
        tick(
            &api.state,
            later,
            later - chrono::Duration::seconds(60),
            false,
        )
        .await
        .expect("the sweep ran");

        assert!(
            !slots_recorded(&api, &id).await.is_empty(),
            "a slot that has come round must be acted on or explained, never ignored: {}",
            api.get(&format!("/v1/runs?task_id={id}")).await
        );
    }

    #[tokio::test]
    async fn a_run_delayed_by_a_few_random_minutes_still_happens() {
        // The failure this guards against: a task set to run "give or take" a
        // few minutes was chosen by the moment it came due but started at the
        // moment plus the delay, so any slot whose delayed start landed after
        // the sweep that considered it fell between two sweeps. It never ran,
        // was never skipped, and left nothing behind at all.
        let api = testkit::start().await;
        let id = a_ready_manual_task(&api).await;
        put_on_schedule_with(
            &api,
            &id,
            json!({
                "kind": "cron", "expr": EVERY_MINUTE, "tz": "UTC",
                "catch_up": "run_once_late", "jitter_s": 300
            }),
        )
        .await;
        let _busy = a_run_in_flight(&api, &id).await;

        // Ten minutes of sweeps, twenty seconds apart, exactly as the daemon
        // does them.
        let start = Utc::now();
        let mut previous = start;
        for step in 1..=30 {
            let now = start + chrono::Duration::seconds(20 * step);
            tick(&api.state, now, previous, false)
                .await
                .expect("the sweep ran");
            previous = now;
        }

        // Every slot in the first five minutes has had its latest possible
        // start pass inside that stretch, so every one of them must have left a
        // record.
        let recorded = slots_recorded(&api, &id).await;
        assert!(
            recorded.len() >= 5,
            "only {} of the first five slots left any trace; a delayed run must still happen or \
             say why not: {:?}",
            recorded.len(),
            recorded
        );
    }

    #[tokio::test]
    async fn a_run_being_made_up_for_waits_its_turn_instead_of_being_thrown_away() {
        // The failure this guards against: a task set to make up every missed
        // run fired the first one and then met its own busy check on the
        // second, which wrote each remaining slot off as skipped. A slot that
        // has been written off can never run, so asking for all of them back
        // delivered exactly one and destroyed the rest, quietly.
        no_real_agent();
        let api = testkit::start().await;
        let id = a_ready_manual_task(&api).await;
        put_on_schedule(&api, &id, EVERY_MINUTE, "run_all").await;
        let busy = a_run_in_flight(&api, &id).await;

        // Five minutes asleep, with a run already going when the machine wakes.
        let later = Utc::now() + chrono::Duration::minutes(5);
        tick(
            &api.state,
            later,
            later - chrono::Duration::minutes(6),
            true,
        )
        .await
        .expect("the sweep ran");

        let runs = api.get(&format!("/v1/runs?task_id={id}")).await;
        assert!(
            runs["items"]
                .as_array()
                .expect("a list of runs")
                .iter()
                .all(|r| r["status"] != "skipped"),
            "a run waiting to be made up for must not be written off because another was still \
             going: {runs}"
        );

        // The run that was in the way finishes, and the queue starts moving.
        errand_core::db::finish_run_ok(&api.pool, &busy, "done", None)
            .await
            .expect("closing the run");
        tick(
            &api.state,
            later + chrono::Duration::seconds(20),
            later,
            false,
        )
        .await
        .expect("the second sweep ran");

        let recorded = slots_recorded(&api, &id).await;
        assert_eq!(
            recorded.len(),
            1,
            "one missed run at a time, so the second cannot start before the first has finished: \
             {recorded:?}"
        );
        // Oldest first: the slot that ran is from the start of the outage, not
        // the end of it.
        let spec = ScheduleSpec::from_json(&api.get(&format!("/v1/tasks/{id}")).await["schedule"])
            .expect("a readable schedule");
        assert!(
            recorded[0] < spec.occurrence_id(later - chrono::Duration::minutes(3)),
            "missed runs are made up for in the order they were missed, oldest first: {recorded:?}"
        );
    }

    #[tokio::test]
    async fn the_time_a_task_promises_to_run_does_not_move_when_the_sweep_looks_at_it() {
        // The failure this guards against: putting a task on a schedule stored
        // the moment it comes due, while the sweep stored the moment it really
        // begins: the same moment plus this task's own small delay. The
        // countdown on the task page therefore jumped the first time the
        // scheduler ticked, by up to a quarter of an hour, with nothing having
        // changed.
        let api = testkit::start().await;
        let id = a_ready_manual_task(&api).await;
        put_on_schedule_with(
            &api,
            &id,
            json!({
                "kind": "cron", "expr": EVERY_MORNING, "tz": "UTC", "jitter_s": 900
            }),
        )
        .await;

        let (code, activated) = api
            .post(&format!("/v1/tasks/{id}/activate"), json!({}))
            .await;
        assert_eq!(code, 200, "activating the task failed: {activated}");
        let task = api.get(&format!("/v1/tasks/{id}")).await;
        let promised = task["next_run_at"].clone();
        assert_eq!(
            activated["next_run_at"], promised,
            "the answer to putting a task on a schedule must be the time it stores"
        );
        assert_eq!(
            task["schedule_preview"][0], promised,
            "the times a task lists must agree with the one it counts down to, or the same run is \
             shown twice at two different times: {task}"
        );

        let now = Utc::now();
        tick(&api.state, now, now - chrono::Duration::seconds(20), false)
            .await
            .expect("the sweep ran");

        assert_eq!(
            api.get(&format!("/v1/tasks/{id}")).await["next_run_at"],
            promised,
            "the time shown must be the time it happens, and must not move because the scheduler \
             looked at it"
        );
    }
}

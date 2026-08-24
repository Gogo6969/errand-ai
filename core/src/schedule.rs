//! When a task should run.
//!
//! All wall-clock reasoning happens in the task's IANA timezone and every
//! instant is stored in UTC. That split matters more than it sounds: a court
//! that opens at 08:00 local opens at a different UTC instant either side of a
//! daylight-saving change, and a scheduler that stores local times drifts by an
//! hour twice a year without anyone noticing until they lose a booking.

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

/// What to do about occurrences that were missed while the Mac was asleep or
/// the daemon was down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CatchUp {
    /// Forget them. Right for anything time-sensitive: nobody wants a court
    /// booked for a slot that has already passed.
    Skip,
    /// Run once if still within the grace window. The sane default.
    #[default]
    RunOnceLate,
    /// Replay every missed occurrence in order. For counter-like tasks where
    /// each run does distinct work.
    RunAll,
}

/// A run window, for tasks where the exact moment matters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    /// Earliest the real work may begin, local time.
    pub not_before: String,
    /// Later than this and the occurrence is abandoned rather than run late.
    pub not_after: String,
    /// How long before `not_before` to start, so the agent is already logged in
    /// and looking at the page when the barrier lifts.
    #[serde(default = "default_arm_early")]
    pub arm_early_s: i64,
}

fn default_arm_early() -> i64 {
    90
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Kind {
    /// Only ever run by hand.
    Manual,
    /// One time, then done.
    Once { at: String },
    /// Repeating, by cron expression.
    Cron { expr: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleSpec {
    #[serde(flatten)]
    pub kind: Kind,
    #[serde(default = "default_tz")]
    pub tz: String,
    #[serde(default)]
    pub jitter_s: i64,
    #[serde(default)]
    pub window: Option<Window>,
    #[serde(default)]
    pub catch_up: CatchUp,
    #[serde(default = "default_grace")]
    pub catch_up_grace_min: i64,
}

fn default_tz() -> String {
    "UTC".to_string()
}

fn default_grace() -> i64 {
    120
}

impl Default for ScheduleSpec {
    fn default() -> Self {
        Self {
            kind: Kind::Manual,
            tz: default_tz(),
            jitter_s: 0,
            window: None,
            catch_up: CatchUp::default(),
            catch_up_grace_min: default_grace(),
        }
    }
}

impl ScheduleSpec {
    pub fn from_json(v: &serde_json::Value) -> Result<Self> {
        serde_json::from_value(v.clone()).context("reading the schedule")
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }

    pub fn timezone(&self) -> Result<Tz> {
        self.tz.parse::<Tz>().map_err(|_| {
            anyhow!(
                "'{}' is not a timezone name. Use something like Europe/Lisbon.",
                self.tz
            )
        })
    }

    pub fn is_scheduled(&self) -> bool {
        !matches!(self.kind, Kind::Manual)
    }

    /// The next moment this task should run, strictly after `after`.
    ///
    /// Returns None for a manual task, and for a one-shot whose moment has
    /// passed.
    pub fn next_after(&self, after: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
        let tz = self.timezone()?;
        match &self.kind {
            Kind::Manual => Ok(None),

            Kind::Once { at } => {
                let instant = parse_local(at, tz)?;
                Ok(if instant > after { Some(instant) } else { None })
            }

            Kind::Cron { expr } => {
                let cron = <croner::Cron as std::str::FromStr>::from_str(expr)
                    .map_err(|e| anyhow!("'{expr}' is not a valid schedule: {e}"))?;
                let local_after = after.with_timezone(&tz);
                let next = cron
                    .find_next_occurrence(&local_after, false)
                    .map_err(|e| anyhow!("could not work out the next run of '{expr}': {e}"))?;
                Ok(Some(next.with_timezone(&Utc)))
            }
        }
    }

    /// A stable identifier for one occurrence.
    ///
    /// This is what the unique index on runs and the side-effect fence key on,
    /// so it must be derived from the scheduled moment rather than from
    /// anything the agent decides later. Minute resolution: two runs of the
    /// same task in the same minute are the same occurrence.
    pub fn occurrence_id(&self, instant: DateTime<Utc>) -> String {
        instant.format("%Y-%m-%dT%H:%MZ").to_string()
    }

    /// When the runner should actually start, given a window that wants the
    /// agent logged in and waiting before the barrier lifts.
    pub fn start_at(&self, occurrence: DateTime<Utc>) -> DateTime<Utc> {
        match &self.window {
            Some(w) => occurrence - Duration::seconds(w.arm_early_s.max(0)),
            None => occurrence,
        }
    }

    /// Has this occurrence's window closed?
    pub fn window_missed(&self, occurrence: DateTime<Utc>, now: DateTime<Utc>) -> Result<bool> {
        let Some(w) = &self.window else {
            return Ok(false);
        };
        let tz = self.timezone()?;
        let local_day = occurrence.with_timezone(&tz).date_naive();
        let not_after = NaiveTime::parse_from_str(&w.not_after, "%H:%M:%S")
            .or_else(|_| NaiveTime::parse_from_str(&w.not_after, "%H:%M"))
            .with_context(|| format!("'{}' is not a time of day", w.not_after))?;
        // Anchor the deadline to the occurrence, not to its calendar day. A
        // window that straddles local midnight (23:55 to 00:15, the shape any
        // "released at midnight" task needs) would otherwise put its deadline
        // most of a day BEFORE the occurrence, and every single run would be
        // abandoned as missed with an explanation that was simply untrue.
        let occ_local = occurrence.with_timezone(&tz).naive_local();
        let mut deadline_local = local_day.and_time(not_after);
        if deadline_local <= occ_local {
            deadline_local += Duration::days(1);
        }
        let deadline = tz
            .from_local_datetime(&deadline_local)
            .earliest()
            .ok_or_else(|| anyhow!("the window's end time does not exist on that day"))?
            .with_timezone(&Utc);
        Ok(now > deadline)
    }

    /// What to do with occurrences that came due while nothing was running.
    ///
    /// Returns the occurrences that should actually be run now, oldest first.
    pub fn catch_up_plan(
        &self,
        missed: &[DateTime<Utc>],
        now: DateTime<Utc>,
    ) -> Vec<DateTime<Utc>> {
        if missed.is_empty() {
            return vec![];
        }
        let grace = Duration::minutes(self.catch_up_grace_min.max(0));
        match self.catch_up {
            CatchUp::Skip => vec![],
            CatchUp::RunAll => missed.to_vec(),
            CatchUp::RunOnceLate => missed
                .iter()
                .rev()
                .find(|m| now - **m <= grace)
                .map(|m| vec![*m])
                .unwrap_or_default(),
        }
    }

    /// Every occurrence between two instants, oldest first. Bounded so a task
    /// that has not run for a year cannot produce an unbounded replay list.
    pub fn occurrences_between(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        max: usize,
    ) -> Result<(Vec<DateTime<Utc>>, usize)> {
        // Keep the NEWEST `max`, not the oldest.
        //
        // Truncating from the front is the wrong end: after a long outage it
        // discards exactly the recent occurrences that catch-up cares about and
        // keeps a pile of ancient ones that are all outside any grace window,
        // so the task silently fails to run late while its policy says it
        // should. The count of what was dropped goes back to the caller so the
        // history can say so instead of leaving a hole.
        let mut out: std::collections::VecDeque<DateTime<Utc>> = Default::default();
        let mut dropped = 0usize;
        let mut cursor = from;
        // A hard ceiling on iterations, so a pathological schedule over a long
        // span cannot spin here.
        for _ in 0..100_000 {
            match self.next_after(cursor)? {
                Some(next) if next <= to => {
                    if next <= cursor {
                        break; // no forward progress; refuse to loop
                    }
                    out.push_back(next);
                    if out.len() > max {
                        out.pop_front();
                        dropped += 1;
                    }
                    cursor = next;
                }
                _ => break,
            }
        }
        Ok((out.into_iter().collect(), dropped))
    }

    /// The most recent occurrence at or before an instant.
    ///
    /// Used for catch-up and for giving a manual run the identity of the slot
    /// it stands in for. Correct however long the outage was, because it does
    /// not enumerate anything.
    pub fn last_occurrence_at_or_before(
        &self,
        instant: DateTime<Utc>,
    ) -> Result<Option<DateTime<Utc>>> {
        match &self.kind {
            Kind::Manual => Ok(None),
            Kind::Once { at } => {
                let i = parse_local(at, self.timezone()?)?;
                Ok(if i <= instant { Some(i) } else { None })
            }
            Kind::Cron { expr } => {
                let cron = <croner::Cron as std::str::FromStr>::from_str(expr)
                    .map_err(|e| anyhow!("'{expr}' is not a valid schedule: {e}"))?;
                let tz = self.timezone()?;
                let local = instant.with_timezone(&tz);
                match cron.find_previous_occurrence(&local, true) {
                    Ok(prev) => Ok(Some(prev.with_timezone(&Utc))),
                    Err(_) => Ok(None),
                }
            }
        }
    }
}

/// Turn a local wall-clock string into an instant, handling the two days a year
/// where a local time is ambiguous or does not exist at all.
fn parse_local(s: &str, tz: Tz) -> Result<DateTime<Utc>> {
    // Accept an explicit offset if given; otherwise treat it as local.
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M"))
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S"))
        .with_context(|| format!("'{s}' is not a date and time"))?;

    match tz.from_local_datetime(&naive) {
        // The ordinary case.
        chrono::LocalResult::Single(dt) => Ok(dt.with_timezone(&Utc)),
        // Clocks went back: this local time happened twice. Take the first,
        // which is the one a person means when they say "at 01:30".
        chrono::LocalResult::Ambiguous(first, _second) => Ok(first.with_timezone(&Utc)),
        // Clocks went forward: this local time never existed. Run at the first
        // instant after the gap rather than skipping the day entirely.
        chrono::LocalResult::None => {
            let mut probe = naive;
            for _ in 0..(4 * 60) {
                probe += Duration::minutes(1);
                if let chrono::LocalResult::Single(dt) = tz.from_local_datetime(&probe) {
                    return Ok(dt.with_timezone(&Utc));
                }
                if let chrono::LocalResult::Ambiguous(dt, _) = tz.from_local_datetime(&probe) {
                    return Ok(dt.with_timezone(&Utc));
                }
            }
            Err(anyhow!(
                "'{s}' does not exist in {tz} and no nearby valid time was found"
            ))
        }
    }
}

/// Deterministic jitter for an occurrence.
///
/// Derived from the task and the moment rather than drawn randomly, so the time
/// shown in the interface is the time it will actually fire, and a restart does
/// not move it.
pub fn jitter_for(task_id: &str, occurrence: DateTime<Utc>, jitter_s: i64) -> Duration {
    if jitter_s <= 0 {
        return Duration::zero();
    }
    let mut h: u64 = 1469598103934665603;
    for b in task_id
        .as_bytes()
        .iter()
        .chain(occurrence.timestamp().to_string().as_bytes().iter())
    {
        h ^= *b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    Duration::seconds((h % (jitter_s as u64 + 1)) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn cron(expr: &str, tz: &str) -> ScheduleSpec {
        ScheduleSpec {
            kind: Kind::Cron {
                expr: expr.to_string(),
            },
            tz: tz.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn manual_tasks_never_come_due() {
        let s = ScheduleSpec::default();
        assert!(!s.is_scheduled());
        assert_eq!(s.next_after(Utc::now()).unwrap(), None);
    }

    #[test]
    fn a_weekly_cron_finds_the_right_weekday() {
        // 08:00 every Wednesday, Lisbon.
        let s = cron("0 0 8 * * WED", "Europe/Lisbon");
        let next = s.next_after(utc("2026-08-24T00:00:00Z")).unwrap().unwrap();
        let local = next.with_timezone(&chrono_tz::Europe::Lisbon);
        assert_eq!(
            local.format("%Y-%m-%d %H:%M").to_string(),
            "2026-08-26 08:00"
        );
    }

    #[test]
    fn the_same_wall_clock_time_is_a_different_instant_across_a_dst_change() {
        // 08:00 Berlin is 06:00 UTC in summer and 07:00 UTC in winter. A
        // scheduler that stored local time would fire an hour wrong for half
        // the year.
        let s = cron("0 0 8 * * *", "Europe/Berlin");
        let summer = s.next_after(utc("2026-07-01T00:00:00Z")).unwrap().unwrap();
        let winter = s.next_after(utc("2026-12-01T00:00:00Z")).unwrap().unwrap();
        assert_eq!(summer.format("%H:%M").to_string(), "06:00");
        assert_eq!(winter.format("%H:%M").to_string(), "07:00");
    }

    #[test]
    fn a_local_time_that_never_existed_runs_just_after_the_gap() {
        // Europe/Berlin springs forward 2026-03-29: 02:00 to 03:00 does not
        // exist. Skipping the day would silently lose a run.
        let s = ScheduleSpec {
            kind: Kind::Once {
                at: "2026-03-29T02:30:00".into(),
            },
            tz: "Europe/Berlin".into(),
            ..Default::default()
        };
        let when = s.next_after(utc("2026-01-01T00:00:00Z")).unwrap().unwrap();
        let local = when.with_timezone(&chrono_tz::Europe::Berlin);
        assert_eq!(local.date_naive().to_string(), "2026-03-29");
        assert!(
            local.format("%H").to_string().parse::<u32>().unwrap() >= 3,
            "expected just after the gap, got {local}"
        );
    }

    #[test]
    fn an_ambiguous_local_time_takes_the_first_occurrence() {
        // Clocks go back 2026-10-25 in Berlin: 02:30 happens twice.
        let s = ScheduleSpec {
            kind: Kind::Once {
                at: "2026-10-25T02:30:00".into(),
            },
            tz: "Europe/Berlin".into(),
            ..Default::default()
        };
        let when = s.next_after(utc("2026-01-01T00:00:00Z")).unwrap().unwrap();
        // The first 02:30 is at 00:30 UTC (CEST, +2); the second at 01:30 UTC.
        assert_eq!(when.format("%H:%M").to_string(), "00:30");
    }

    #[test]
    fn a_one_shot_in_the_past_never_comes_due_again() {
        let s = ScheduleSpec {
            kind: Kind::Once {
                at: "2020-01-01T10:00:00".into(),
            },
            tz: "UTC".into(),
            ..Default::default()
        };
        assert_eq!(s.next_after(Utc::now()).unwrap(), None);
    }

    #[test]
    fn occurrence_ids_are_stable_and_minute_resolution() {
        let s = cron("0 0 8 * * *", "UTC");
        let a = s.occurrence_id(utc("2026-08-26T08:00:00Z"));
        let b = s.occurrence_id(utc("2026-08-26T08:00:59Z"));
        assert_eq!(a, b, "the same minute is the same occurrence");
        assert_ne!(a, s.occurrence_id(utc("2026-08-26T08:01:00Z")));
    }

    #[test]
    fn a_window_starts_the_run_early_enough_to_be_logged_in() {
        let s = ScheduleSpec {
            kind: Kind::Cron {
                expr: "0 0 8 * * WED".into(),
            },
            window: Some(Window {
                not_before: "08:00:00".into(),
                not_after: "08:10:00".into(),
                arm_early_s: 90,
            }),
            ..Default::default()
        };
        let occ = utc("2026-08-26T08:00:00Z");
        assert_eq!(s.start_at(occ), utc("2026-08-26T07:58:30Z"));
    }

    #[test]
    fn a_closed_window_is_abandoned_rather_than_run_late() {
        let s = ScheduleSpec {
            kind: Kind::Cron {
                expr: "0 0 8 * * *".into(),
            },
            tz: "UTC".into(),
            window: Some(Window {
                not_before: "08:00:00".into(),
                not_after: "08:10:00".into(),
                arm_early_s: 90,
            }),
            ..Default::default()
        };
        let occ = utc("2026-08-26T08:00:00Z");
        assert!(!s.window_missed(occ, utc("2026-08-26T08:05:00Z")).unwrap());
        assert!(s.window_missed(occ, utc("2026-08-26T08:30:00Z")).unwrap());
    }

    #[test]
    fn skip_forgets_everything_missed() {
        let s = ScheduleSpec {
            catch_up: CatchUp::Skip,
            ..cron("0 0 8 * * *", "UTC")
        };
        let missed = vec![utc("2026-08-24T08:00:00Z"), utc("2026-08-25T08:00:00Z")];
        assert!(s
            .catch_up_plan(&missed, utc("2026-08-25T09:00:00Z"))
            .is_empty());
    }

    #[test]
    fn run_once_late_takes_only_the_most_recent_and_only_within_grace() {
        let s = ScheduleSpec {
            catch_up: CatchUp::RunOnceLate,
            catch_up_grace_min: 120,
            ..cron("0 0 8 * * *", "UTC")
        };
        let missed = vec![utc("2026-08-24T08:00:00Z"), utc("2026-08-25T08:00:00Z")];

        // One hour late: run the most recent one only.
        let plan = s.catch_up_plan(&missed, utc("2026-08-25T09:00:00Z"));
        assert_eq!(plan, vec![utc("2026-08-25T08:00:00Z")]);

        // Six hours late: past grace, so nothing. Nobody wants a court booked
        // for a slot that has already gone.
        assert!(s
            .catch_up_plan(&missed, utc("2026-08-25T14:00:00Z"))
            .is_empty());
    }

    #[test]
    fn run_all_replays_every_missed_occurrence_in_order() {
        let s = ScheduleSpec {
            catch_up: CatchUp::RunAll,
            ..cron("0 0 8 * * *", "UTC")
        };
        let missed = vec![utc("2026-08-24T08:00:00Z"), utc("2026-08-25T08:00:00Z")];
        assert_eq!(
            s.catch_up_plan(&missed, utc("2026-08-25T09:00:00Z")),
            missed
        );
    }

    #[test]
    fn replaying_a_long_outage_is_bounded() {
        let s = cron("0 0 8 * * *", "UTC");
        let (list, dropped) = s
            .occurrences_between(utc("2020-01-01T00:00:00Z"), utc("2026-01-01T00:00:00Z"), 10)
            .unwrap();
        assert_eq!(
            list.len(),
            10,
            "a long gap must not produce an unbounded list"
        );
        assert!(dropped > 0, "the caller must be told what was discarded");
        // Crucially it kept the NEWEST ten. Truncating from the other end would
        // discard exactly the recent occurrences catch-up cares about and keep
        // a pile of ancient ones that are all outside any grace window.
        assert!(
            list.last().unwrap() > &utc("2025-12-01T00:00:00Z"),
            "truncation kept the wrong end: {:?}",
            list.last()
        );
    }

    #[test]
    fn jitter_is_stable_for_an_occurrence_and_within_bounds() {
        let occ = utc("2026-08-26T08:00:00Z");
        let a = jitter_for("task-1", occ, 60);
        let b = jitter_for("task-1", occ, 60);
        assert_eq!(a, b, "the time shown must be the time it fires");
        assert!(a >= Duration::zero() && a <= Duration::seconds(60));
        assert_eq!(jitter_for("task-1", occ, 0), Duration::zero());
    }

    #[test]
    fn a_bad_timezone_is_reported_in_plain_language() {
        let s = cron("0 0 8 * * *", "Mars/Olympus");
        let e = s.next_after(Utc::now()).unwrap_err().to_string();
        assert!(e.contains("not a timezone"), "unhelpful error: {e}");
    }

    #[test]
    fn a_bad_cron_expression_is_reported_in_plain_language() {
        let s = cron("not a cron", "UTC");
        let e = s.next_after(Utc::now()).unwrap_err().to_string();
        assert!(e.contains("not a valid schedule"), "unhelpful error: {e}");
    }

    #[test]
    fn json_round_trips_through_the_stored_shape() {
        let s = ScheduleSpec {
            kind: Kind::Cron {
                expr: "0 0 8 * * WED".into(),
            },
            tz: "Europe/Lisbon".into(),
            jitter_s: 30,
            window: Some(Window {
                not_before: "08:00:00".into(),
                not_after: "08:10:00".into(),
                arm_early_s: 90,
            }),
            catch_up: CatchUp::RunOnceLate,
            catch_up_grace_min: 120,
        };
        let back = ScheduleSpec::from_json(&s.to_json()).unwrap();
        assert_eq!(back.tz, s.tz);
        assert_eq!(back.jitter_s, 30);
        assert_eq!(back.catch_up, CatchUp::RunOnceLate);
        assert_eq!(back.window, s.window);
    }

    #[test]
    fn a_manual_schedule_is_the_default_for_an_unknown_shape() {
        let v = serde_json::json!({ "kind": "manual" });
        let s = ScheduleSpec::from_json(&v).unwrap();
        assert!(!s.is_scheduled());
        assert_eq!(s.catch_up, CatchUp::RunOnceLate);
    }

    #[test]
    fn a_window_across_local_midnight_is_not_missed_the_moment_it_opens() {
        // Anything released at midnight needs this shape. Anchoring the
        // deadline to the occurrence's own calendar day would put it nearly a
        // day in the past and abandon every single run.
        let s = ScheduleSpec {
            kind: Kind::Cron {
                expr: "0 55 23 * * *".into(),
            },
            tz: "UTC".into(),
            window: Some(Window {
                not_before: "23:55:00".into(),
                not_after: "00:15:00".into(),
                arm_early_s: 60,
            }),
            ..Default::default()
        };
        let occ = utc("2026-08-26T23:55:00Z");
        assert!(
            !s.window_missed(occ, occ).unwrap(),
            "missed at the instant it opened"
        );
        assert!(
            !s.window_missed(occ, utc("2026-08-27T00:05:00Z")).unwrap(),
            "still inside"
        );
        assert!(
            s.window_missed(occ, utc("2026-08-27T00:30:00Z")).unwrap(),
            "should be closed"
        );
    }

    #[test]
    fn the_previous_occurrence_is_found_without_enumerating() {
        let s = cron("0 0 8 * * *", "UTC");
        let prev = s
            .last_occurrence_at_or_before(utc("2026-08-26T09:00:00Z"))
            .unwrap()
            .unwrap();
        assert_eq!(prev, utc("2026-08-26T08:00:00Z"));

        // Even across a decade-long gap, which is the point.
        let old = s
            .last_occurrence_at_or_before(utc("2036-01-01T12:00:00Z"))
            .unwrap()
            .unwrap();
        assert_eq!(old.format("%H:%M").to_string(), "08:00");
    }

    #[test]
    fn a_manual_task_has_no_previous_occurrence() {
        assert_eq!(
            ScheduleSpec::default()
                .last_occurrence_at_or_before(Utc::now())
                .unwrap(),
            None
        );
    }
}

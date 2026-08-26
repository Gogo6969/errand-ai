//! What a single run is allowed to spend.
//!
//! An agent with a browser and no ceiling can loop on a page that never quite
//! loads until it has spent real money and hours. Every limit here exists
//! because the alternative is discovering the number on a bill.
//!
//! Breaching a limit is a terminal failure with its own code, so the run stops
//! and says which ceiling it hit rather than being quietly killed.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    /// Journal steps, which is roughly "things the agent did".
    #[serde(default = "d_steps")]
    pub max_steps: i64,
    /// Wall clock for the whole run.
    #[serde(default = "d_minutes")]
    pub max_minutes: i64,
    /// What the models for this run may cost.
    #[serde(default = "d_usd")]
    pub max_usd: f64,
    /// How many times the fixer may try to repair a broken approach.
    #[serde(default = "d_heal")]
    pub max_heal_cycles: i64,
    /// Messages to real people. A bug that messages someone repeatedly is worse
    /// than one that fails.
    #[serde(default = "d_messages")]
    pub max_messages: i64,
}

fn d_steps() -> i64 {
    60
}
fn d_minutes() -> i64 {
    15
}
fn d_usd() -> f64 {
    0.50
}
fn d_heal() -> i64 {
    2
}
/// How many times in a row a task may fail before it stops running itself.
///
/// Not a per-run ceiling like the others: this is the one that stops a task
/// from failing the same way every hour for a week. Three, because two can be
/// a site having a bad afternoon and three is a pattern. It only stops the
/// scheduled runs; pressing Run now still works, which is what somebody
/// fixing it will be doing.
pub const FAILURES_BEFORE_PAUSING: i64 = 3;

fn d_messages() -> i64 {
    3
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_steps: d_steps(),
            max_minutes: d_minutes(),
            max_usd: d_usd(),
            max_heal_cycles: d_heal(),
            max_messages: d_messages(),
        }
    }
}

/// Which ceiling a run hit, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Breach {
    Steps,
    Minutes,
    Cost,
    Messages,
}

impl Breach {
    /// What to tell the person. Names the limit and what to do, because "budget
    /// exceeded" on its own tells them nothing they can act on.
    pub fn explain(&self, limits: &Limits) -> String {
        match self {
            Self::Steps => format!(
                "It took more than {} separate actions without finishing. That usually means the \
                 site changed and it was going round in circles. Look at the run to see where it \
                 got stuck, or raise the step limit for this task.",
                limits.max_steps
            ),
            Self::Minutes => format!(
                "It ran for more than {} minutes without finishing, so it was stopped. Either the \
                 site was unusually slow, or it was waiting for something that was never going to \
                 happen.",
                limits.max_minutes
            ),
            Self::Cost => format!(
                "It reached this task's spending limit of ${:.2} without finishing. Raise the \
                 limit if the task genuinely needs more, but check the run first: a task that \
                 costs more than expected is usually stuck rather than busy.",
                limits.max_usd
            ),
            Self::Messages => format!(
                "It tried to send more than {} messages in one run, which is almost always a bug \
                 rather than an intention. Nothing further was sent.",
                limits.max_messages
            ),
        }
    }
}

impl Limits {
    pub fn from_json(v: &serde_json::Value) -> Self {
        serde_json::from_value(v.clone()).unwrap_or_default()
    }

    /// Has this run gone past anything it was allowed?
    pub fn check(
        &self,
        steps: i64,
        elapsed_s: i64,
        cost_usd: f64,
        messages: i64,
    ) -> Option<Breach> {
        if self.max_steps > 0 && steps > self.max_steps {
            return Some(Breach::Steps);
        }
        if self.max_minutes > 0 && elapsed_s > self.max_minutes * 60 {
            return Some(Breach::Minutes);
        }
        if self.max_usd > 0.0 && cost_usd > self.max_usd {
            return Some(Breach::Cost);
        }
        if self.max_messages > 0 && messages > self.max_messages {
            return Some(Breach::Messages);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_inside_every_limit_is_left_alone() {
        let l = Limits::default();
        assert_eq!(l.check(10, 60, 0.02, 0), None);
        // Exactly at the limit is still allowed; only past it is a breach.
        assert_eq!(l.check(60, 15 * 60, 0.50, 3), None);
    }

    #[test]
    fn each_ceiling_is_reported_as_itself() {
        let l = Limits::default();
        assert_eq!(l.check(61, 0, 0.0, 0), Some(Breach::Steps));
        assert_eq!(l.check(0, 15 * 60 + 1, 0.0, 0), Some(Breach::Minutes));
        assert_eq!(l.check(0, 0, 0.51, 0), Some(Breach::Cost));
        assert_eq!(l.check(0, 0, 0.0, 4), Some(Breach::Messages));
    }

    #[test]
    fn the_explanation_names_the_number_and_what_to_do() {
        let l = Limits::default();
        let cost = Breach::Cost.explain(&l);
        assert!(cost.contains("$0.50"));
        assert!(cost.contains("stuck rather than busy"));
        assert!(Breach::Steps.explain(&l).contains("60"));
    }

    #[test]
    fn a_zero_limit_means_no_limit_rather_than_instant_failure() {
        let l = Limits {
            max_steps: 0,
            max_minutes: 0,
            max_usd: 0.0,
            max_messages: 0,
            ..Default::default()
        };
        assert_eq!(l.check(10_000, 10_000, 999.0, 999), None);
    }

    #[test]
    fn an_unreadable_limits_column_falls_back_to_the_defaults() {
        // Better a sane ceiling than none at all.
        let l = Limits::from_json(&serde_json::json!("nonsense"));
        assert_eq!(l, Limits::default());
        let partial = Limits::from_json(&serde_json::json!({ "max_usd": 2.0 }));
        assert_eq!(partial.max_usd, 2.0);
        assert_eq!(partial.max_steps, 60);
    }
}

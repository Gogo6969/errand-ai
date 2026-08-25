//! Trying again, differently.
//!
//! When a run fails because the site changed rather than because the job is
//! impossible, retrying the same steps achieves nothing. The Fixer reads what
//! actually happened, works out what it thinks went wrong, and proposes a
//! different way in. The next attempt runs with that advice in front of it.
//!
//! Two things make this safe to do at all. The attempt is a fresh run rather
//! than a resumption, which avoids reasoning about half-finished state; and the
//! side-effect fence means a repeat cannot redo anything irreversible that the
//! failed attempt already committed. Without the fence this would be reckless.
//!
//! The Fixer never edits the playbook. It advises one attempt. Anything worth
//! keeping goes through the same human approval as everything else, because a
//! failure diagnosed from a hostile page is not a trustworthy source of
//! permanent instructions.

use errand_core::models::{FailureCode, RetryClass};

use crate::state::AppState;

/// What the Fixer concluded.
#[derive(Debug, Clone)]
pub struct Diagnosis {
    /// What it believes went wrong, in plain language.
    pub cause: String,
    /// What to try instead.
    pub advice: String,
    /// Which AI worked this out, so the run can say so rather than presenting
    /// an anonymous verdict.
    pub by: String,
}

impl Diagnosis {
    /// The text handed to the next attempt.
    pub fn as_prompt(&self) -> String {
        format!(
            "A previous attempt at this task failed. Something looked at what happened and \
             concluded:\n\nWhat went wrong: {}\n\nWhat to try instead: {}\n\nTreat that as a lead, \
             not as fact. If the page in front of you contradicts it, believe the page. If this \
             approach fails too, stop and say so rather than trying a third variation.",
            self.cause, self.advice
        )
    }
}

/// Should a failure be retried at all, and how?
///
/// Judged from the taxonomy rather than from how the failure felt, so the
/// decision is the same every time.
pub fn retry_plan(code: FailureCode, heal_cycles: i64, max_heal: i64) -> Retry {
    match code.retry_class() {
        // Nothing was wrong with the approach, so the same approach may work.
        RetryClass::Transient => Retry::Again,
        RetryClass::Healable => {
            if heal_cycles < max_heal {
                Retry::AfterDiagnosis
            } else {
                // Repeating a diagnosis that has already failed twice is how a
                // run burns a budget without getting anywhere.
                Retry::No("it has already tried a different approach and that failed too")
            }
        }
        RetryClass::Terminal => Retry::No("trying again would hit the same wall"),
        RetryClass::None => Retry::No("nothing was attempted"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Retry {
    /// Run it again unchanged.
    Again,
    /// Ask the Fixer first, then run with its advice.
    AfterDiagnosis,
    No(&'static str),
}

/// Ask a model what went wrong and what to try instead.
///
/// Deliberately a small, cheap, tightly bounded call: it reads a summary rather
/// than driving anything, and it gets no tools at all.
pub async fn diagnose(state: &AppState, run_id: &str) -> anyhow::Result<Diagnosis> {
    let run = errand_core::db::get_run(state.pool(), run_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("run not found"))?;
    let task = errand_core::db::get_task(state.pool(), &run.task_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("task not found"))?;
    let steps = errand_core::db::list_steps(state.pool(), run_id).await?;

    // The tail is what matters: the beginning of a run is almost always fine.
    let tail: Vec<String> = steps
        .iter()
        .rev()
        .take(15)
        .rev()
        .map(|s| format!("{} [{}] {}", s.seq, s.kind, s.title))
        .collect();

    let failure = run
        .failure
        .as_ref()
        .map(|f| format!("{}: {}", f.code, f.plain_reason))
        .unwrap_or_else(|| "unknown".into());

    let prompt = format!(
        "A task tried to do this and failed. Work out what went wrong and what is worth trying \
         instead.\n\n\
         The task: {}\n\n\
         What the person asked for:\n{}\n\n\
         How it reported the failure:\n{}\n\n\
         The last things it did:\n{}\n\n\
         Answer in exactly two lines, nothing else:\n\
         CAUSE: one sentence on what actually went wrong\n\
         TRY: one sentence on what to do differently\n\n\
         If the failure looks like the job is genuinely not possible right now, say so in CAUSE \
         and put 'nothing worth retrying' in TRY. Do not invent a way past a login wall, a \
         payment step or a human check.",
        task.name,
        task.description,
        failure,
        tail.join("\n")
    );

    // Scrubbed, because the journal can quote a page that contained a secret.
    let prompt = state.redactor(run_id).scrub(&prompt);
    // Through the provider chain, so a local model set for this job is really
    // used and a diagnosis can name what produced it.
    let answer = crate::models::ask(state, errand_core::providers::Role::Fixer, &prompt)
        .await
        .map_err(|e| crate::executor::ExecError::NoModel(e.to_string()))?;

    let mut d = parse(&answer.text);
    // Named, and said where it ran. A guess about your failed booking is worth
    // knowing the origin of, including whether it left the machine.
    d.by = format!(
        "{} ({}{})",
        answer.provider_label,
        answer.model,
        if answer.was_local {
            ", on this machine"
        } else {
            ""
        }
    );
    Ok(d)
}

/// Pull the two lines out, tolerantly. A model that ignores the format should
/// degrade to something usable rather than to an error.
pub fn parse(raw: &str) -> Diagnosis {
    let mut cause = String::new();
    let mut advice = String::new();
    for line in raw.lines() {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("CAUSE:") {
            cause = v.trim().to_string();
        } else if let Some(v) = t.strip_prefix("TRY:") {
            advice = v.trim().to_string();
        }
    }
    if cause.is_empty() {
        cause = raw
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim()
            .to_string();
    }
    if advice.is_empty() {
        advice = "nothing worth retrying".to_string();
    }
    Diagnosis {
        cause,
        advice,
        // Filled in by whatever asked; a hand-parsed answer has no author yet.
        by: String::new(),
    }
}

impl Diagnosis {
    /// Did the Fixer conclude there is nothing to try?
    pub fn is_hopeless(&self) -> bool {
        let a = self.advice.to_ascii_lowercase();
        a.contains("nothing worth retrying") || a.trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transient_failure_is_simply_retried() {
        assert_eq!(retry_plan(FailureCode::Network, 0, 2), Retry::Again);
        assert_eq!(retry_plan(FailureCode::ProviderError, 0, 2), Retry::Again);
    }

    #[test]
    fn a_changed_page_is_retried_only_with_new_advice() {
        assert_eq!(
            retry_plan(FailureCode::UiChanged, 0, 2),
            Retry::AfterDiagnosis
        );
    }

    #[test]
    fn healing_stops_once_it_has_had_its_chances() {
        // Repeating a diagnosis that already failed is how a run burns a budget
        // without getting anywhere.
        assert!(matches!(
            retry_plan(FailureCode::UiChanged, 2, 2),
            Retry::No(_)
        ));
    }

    #[test]
    fn a_wall_is_never_retried() {
        for code in [
            FailureCode::AuthExpired,
            FailureCode::CaptchaOr2faNeeded,
            FailureCode::TargetUnavailable,
            FailureCode::BudgetExceeded,
            FailureCode::NeedsHumanDecision,
            FailureCode::ContainmentBreach,
        ] {
            assert!(
                matches!(retry_plan(code, 0, 2), Retry::No(_)),
                "{code:?} must not be retried automatically"
            );
        }
    }

    #[test]
    fn the_two_lines_are_pulled_out() {
        let d = parse("CAUSE: the login button moved into a menu\nTRY: open the menu first");
        assert_eq!(d.cause, "the login button moved into a menu");
        assert_eq!(d.advice, "open the menu first");
        assert!(!d.is_hopeless());
    }

    #[test]
    fn a_model_that_ignores_the_format_still_gives_something_usable() {
        let d = parse("The site seems to be down entirely.");
        assert_eq!(d.cause, "The site seems to be down entirely.");
        assert!(d.is_hopeless(), "with no advice there is nothing to retry");
    }

    #[test]
    fn a_hopeless_verdict_is_recognised() {
        let d = parse("CAUSE: the account is locked\nTRY: nothing worth retrying");
        assert!(d.is_hopeless());
    }

    #[test]
    fn the_advice_is_offered_as_a_lead_rather_than_as_fact() {
        let d = parse("CAUSE: a\nTRY: b");
        let p = d.as_prompt();
        assert!(p.contains("believe the page"));
        assert!(p.contains("stop and say so"));
    }
}

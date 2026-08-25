//! Writing down what a run learned, when the agent did not.
//!
//! Normally the agent writes its own plan: it calls `save_playbook` near the end
//! of a run, and it is the right thing to do it because it is the only thing
//! that watched the run happen: it knows why it clicked what it clicked, which
//! is the difference between an intent and a hint.
//!
//! But an agent that simply forgets leaves nothing at all, and a task with no
//! plan can never be armed. Before this existed the only remedy was to teach it
//! again and hope for better, which is a poor answer to "it worked, why can I
//! not use it?".
//!
//! So when a run ends having done real work and left no plan behind, the plan is
//! distilled from the journal instead. It is a second best and it is labelled as
//! one: the journal records what happened, not what was intended, so the intents
//! it produces are inferred. Nothing about the approval gate changes: a
//! distilled plan is written unapproved, and a person still reads it before
//! anything runs alone.

use errand_core::playbook::{Playbook, Source, Step};
use serde::Deserialize;

use crate::state::AppState;

/// The shape the model is asked for. Deliberately the same fields the agent's
/// own `save_playbook` tool takes, so the two produce comparable plans.
#[derive(Debug, Deserialize)]
struct Draft {
    goal: String,
    #[serde(default)]
    steps: Vec<DraftStep>,
    #[serde(default)]
    preconditions: Vec<String>,
    #[serde(default)]
    success: Vec<String>,
    #[serde(default)]
    known_failures: Vec<String>,
    #[serde(default)]
    never: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct DraftStep {
    intent: String,
    #[serde(default)]
    hint: Option<String>,
    #[serde(default)]
    decision: Option<String>,
}

/// Write a plan from the journal, but only if the run left none.
///
/// Never fails a run. A run that did its job and then could not have its plan
/// written is still a run that did its job, and saying otherwise would be a
/// worse lie than having no plan.
pub async fn distil_if_missing(state: &AppState, run_id: &str) {
    match distil(state, run_id).await {
        Ok(Some(version)) => {
            tracing::info!(
                run_id,
                version,
                "wrote a plan from the journal for approval"
            )
        }
        Ok(None) => {}
        Err(e) => tracing::info!(run_id, "could not write a plan from the journal: {e}"),
    }
}

async fn distil(state: &AppState, run_id: &str) -> anyhow::Result<Option<i64>> {
    let Some(run) = errand_core::db::get_run(state.pool(), run_id).await? else {
        return Ok(None);
    };
    // A rehearsal deliberately does nothing irreversible, so what it learned is
    // not what a real run would learn.
    if run.mode == "dry_run" {
        return Ok(None);
    }
    let Some(task) = errand_core::db::get_task(state.pool(), &run.task_id).await? else {
        return Ok(None);
    };

    // The agent's own plan always wins. This is the fallback, not a second
    // opinion: two plans for one run is a choice nobody asked to make.
    if errand_core::db::playbook_written_by_run(state.pool(), run_id).await? {
        return Ok(None);
    }
    // And there is nothing to fix if the task already has a plan it can follow.
    if errand_core::db::active_playbook(state.pool(), &run.task_id)
        .await?
        .is_some()
    {
        return Ok(None);
    }

    let steps = errand_core::db::list_steps(state.pool(), run_id).await?;
    // A run that did almost nothing has nothing worth distilling, and asking a
    // model to invent a plan from three lines is how a confident fiction gets
    // written down and then followed at eight in the morning.
    if steps.iter().filter(|s| s.ok).count() < 4 {
        return Ok(None);
    }

    let journal: Vec<String> = steps
        .iter()
        .map(|s| {
            format!(
                "{} [{}]{} {}",
                s.seq,
                s.kind,
                if s.ok { "" } else { " (failed)" },
                s.title
            )
        })
        .collect();

    let sites: Vec<String> = task
        .allowed_domains
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|d| d.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let prompt = format!(
        "A task was carried out successfully and whoever did it did not write down how. Write \
         that down now, from the record of what actually happened, so the next run does not start \
         from nothing.\n\n\
         What the person asked for:\n{}\n\n\
         What was actually done, in order:\n{}\n\n\
         Reply with JSON and nothing else, in this shape:\n\
         {{\"goal\": \"one sentence: what this task achieves\",\n\
          \"steps\": [{{\"intent\": \"what this step achieves\", \"hint\": \"how it was done this \
         time\", \"decision\": \"what to do if the obvious path is missing\"}}],\n\
          \"preconditions\": [], \"success\": [\"how a future run knows it really worked\"],\n\
          \"known_failures\": [], \"never\": [\"things a future run must never do\"]}}\n\n\
         Rules. Describe only what the record shows: do not invent a step, a page, a price or a \
         confirmation that is not above. Separate the INTENT of each step from the HINT of how it \
         was done, because sites move their buttons and intentions do not. Where the record does \
         not say why something was done, write the intent as what it plainly achieved rather than \
         guessing at a motive. Leave a list empty rather than filling it with something plausible.",
        task.description,
        journal.join("\n"),
    );

    // Scrubbed like everything else that reaches a model: a journal can quote a
    // page that had a secret on it.
    let prompt = state.redactor(run_id).scrub(&prompt);

    let answer = crate::models::ask(state, errand_core::providers::Role::Planner, &prompt).await?;
    let draft: Draft = parse_draft(&answer.text)?;

    if draft.goal.trim().is_empty() || draft.steps.is_empty() {
        anyhow::bail!("the model did not describe a usable plan");
    }

    let red = state.redactor(run_id);
    let version = errand_core::db::next_playbook_version(state.pool(), &run.task_id).await?;
    let pb = Playbook {
        version,
        goal: red.scrub(draft.goal.trim()),
        sites,
        preconditions: draft.preconditions.iter().map(|s| red.scrub(s)).collect(),
        steps: draft
            .steps
            .into_iter()
            .map(|s| Step {
                intent: red.scrub(s.intent.trim()),
                hint: s.hint.map(|h| red.scrub(&h)),
                decision: s.decision.map(|d| red.scrub(&d)),
            })
            .collect(),
        success: draft.success.iter().map(|s| red.scrub(s)).collect(),
        known_failures: draft.known_failures.iter().map(|s| red.scrub(s)).collect(),
        never: draft.never.iter().map(|s| red.scrub(s)).collect(),
    };

    // Teach for a supervised first run, refine otherwise, the same distinction
    // the agent's own tool draws, so the history reads consistently.
    let source = if run.mode == "teach" {
        Source::Teach
    } else {
        Source::Refine
    };
    let changelog = format!(
        "Written from the record of run {} by {}, because the run left no plan of its own. The \
         steps below are inferred from what happened rather than described by whoever did it, so \
         read them before approving.",
        &run_id[..run_id.len().min(8)],
        answer.provider_label,
    );

    let version = errand_core::db::add_playbook_version(
        state.pool(),
        &run.task_id,
        &pb,
        source,
        Some(run_id),
        Some(&changelog),
        // Never approved. A plan nobody read is exactly what the approval gate
        // exists to prevent, and one this program wrote itself is no exception.
        false,
    )
    .await?;

    Ok(Some(version))
}

/// Pull the JSON out of whatever the model replied with.
///
/// Models wrap JSON in prose and in code fences however firmly they are asked
/// not to, and refusing a good plan over a pair of backticks would be a poor
/// trade.
fn parse_draft(raw: &str) -> anyhow::Result<Draft> {
    let text = raw.trim();
    let body = match (text.find('{'), text.rfind('}')) {
        (Some(a), Some(b)) if b > a => &text[a..=b],
        _ => anyhow::bail!("the reply contained no plan"),
    };
    Ok(serde_json::from_str(body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_wrapped_in_prose_or_fences_is_still_read() {
        let fenced = "Here you go:\n```json\n{\"goal\":\"book a court\",\
                      \"steps\":[{\"intent\":\"sign in\"}]}\n```\nHope that helps.";
        let d = parse_draft(fenced).expect("should find the plan");
        assert_eq!(d.goal, "book a court");
        assert_eq!(d.steps.len(), 1);
        assert_eq!(d.steps[0].intent, "sign in");
    }

    #[test]
    fn a_reply_with_no_plan_in_it_is_an_error_rather_than_an_empty_plan() {
        // An empty plan would be saved, shown for approval, and mean nothing.
        assert!(parse_draft("I could not work out what happened.").is_err());
        assert!(parse_draft("").is_err());
        assert!(parse_draft("{ not json at all }").is_err());
    }

    #[test]
    fn the_parts_a_model_leaves_out_come_back_empty_rather_than_failing() {
        // Only a goal and steps are required; a model that omits the optional
        // lists has still written something worth reading.
        let d = parse_draft(r#"{"goal":"g","steps":[{"intent":"i"}]}"#).unwrap();
        assert!(d.preconditions.is_empty());
        assert!(d.never.is_empty());
        assert_eq!(d.steps[0].hint, None);
    }
}

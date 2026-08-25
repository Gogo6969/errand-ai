//! The queue every message leaves through.
//!
//! Nothing sends inline. A run finishing enqueues a row and moves on, because
//! whether a message got through is a different question from whether the work
//! happened, and letting the first answer change the second is how a successful
//! booking gets recorded as a failure because Telegram was slow.
//!
//! Rows survive a restart, so a crash between doing the work and reporting it
//! means the report is late rather than lost.

use crate::channels::{self, ChannelError, ChannelId, Outbound};
use crate::state::AppState;

/// How long to wait before each attempt. After the last one it is given up on,
/// and the row stays visible rather than disappearing.
const BACKOFF_S: &[u64] = &[30, 120, 600, 1800, 7200];

const TICK: std::time::Duration = std::time::Duration::from_secs(5);

pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        // Anything left mid-send by a crash is in an unknown state: it may have
        // arrived. Sweeping it to `uncertain` rather than retrying is the
        // difference between a duplicate message and an honest one.
        if let Err(e) = sweep_uncertain(&state).await {
            tracing::warn!("could not sweep interrupted sends: {e}");
        }
        loop {
            tokio::time::sleep(TICK).await;
            if let Err(e) = tick(&state).await {
                tracing::error!("outbox tick failed: {e}");
            }
        }
    });
}

async fn sweep_uncertain(state: &AppState) -> anyhow::Result<()> {
    let n = errand_core::db::mark_sending_uncertain(state.pool()).await?;
    if n > 0 {
        tracing::warn!(
            count = n,
            "messages were in flight when Errand stopped; they may or may not have arrived"
        );
    }
    Ok(())
}

async fn tick(state: &AppState) -> anyhow::Result<()> {
    let due = errand_core::db::due_outbox(state.pool(), 10).await?;
    for row in due {
        let Some(channel) = ChannelId::parse(&row.channel) else {
            errand_core::db::fail_outbox(
                state.pool(),
                &row.id,
                "dead",
                &format!("'{}' is not a channel Errand knows", row.channel),
                None,
            )
            .await?;
            continue;
        };

        // Quiet hours apply to people, and to good news. Bad news you asked to
        // hear about breaks through, because being told at 09:00 that the 08:00
        // booking failed is being told too late to act.
        if row.class != "test" {
            let hour = chrono::Local::now()
                .format("%H")
                .to_string()
                .parse()
                .unwrap_or(12);
            let quiet = quiet_hours(state).await;
            if let Some((from, to, breaks)) = quiet {
                if channels::deferred_until(hour, from, to, row.is_failure, breaks) {
                    errand_core::db::defer_outbox(state.pool(), &row.id, next_hour_utc(to)).await?;
                    continue;
                }
            }
        }

        errand_core::db::begin_send(state.pool(), &row.id).await?;

        let msg = Outbound {
            channel,
            recipient: row.recipient.clone(),
            subject: row.subject.clone(),
            body: row.body.clone(),
        };

        match channels::send(state.pool(), &msg).await {
            Ok(receipt) => {
                errand_core::db::mark_sent(state.pool(), &row.id, &receipt).await?;
                tracing::info!(id = %row.id, channel = %row.channel, "message sent");
            }
            Err(e) => {
                let attempts = row.attempts + 1;
                match e {
                    ChannelError::Permanent(ref m) => {
                        errand_core::db::fail_outbox(state.pool(), &row.id, "dead", m, None)
                            .await?;
                    }
                    ChannelError::NeedsUser { ref why, ref fix } => {
                        // Parked rather than retried. Retrying achieves nothing
                        // until a person does something, and a queue that keeps
                        // trying hides the fact that it needs them.
                        errand_core::db::fail_outbox(
                            state.pool(),
                            &row.id,
                            "needs_user",
                            &format!("{why}. {fix}"),
                            None,
                        )
                        .await?;
                    }
                    ChannelError::RateLimited(d) => {
                        errand_core::db::fail_outbox(
                            state.pool(),
                            &row.id,
                            "retry_wait",
                            "rate limited",
                            Some(iso_in(d.as_secs() as i64)),
                        )
                        .await?;
                    }
                    ChannelError::Transient(ref m) => match BACKOFF_S.get(attempts as usize - 1) {
                        Some(secs) => {
                            errand_core::db::fail_outbox(
                                state.pool(),
                                &row.id,
                                "retry_wait",
                                m,
                                Some(iso_in(*secs as i64)),
                            )
                            .await?;
                        }
                        None => {
                            errand_core::db::fail_outbox(
                                state.pool(),
                                &row.id,
                                "dead",
                                &format!("gave up after {attempts} attempts: {m}"),
                                None,
                            )
                            .await?;
                        }
                    },
                }
            }
        }
    }
    Ok(())
}

async fn quiet_hours(state: &AppState) -> Option<(u32, u32, bool)> {
    let v = errand_core::db::get_setting(state.pool(), "messaging.quiet")
        .await
        .ok()
        .flatten()?;
    let from = v.get("from")?.as_u64()? as u32;
    let to = v.get("to")?.as_u64()? as u32;
    let breaks = v
        .get("failure_breaks_through")
        .and_then(|b| b.as_bool())
        .unwrap_or(true);
    Some((from, to, breaks))
}

fn iso_in(secs: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(secs)).to_rfc3339()
}

fn next_hour_utc(local_hour: u32) -> String {
    // Good enough: wake at the next occurrence of that local hour.
    let now = chrono::Local::now();
    let mut t = now
        .date_naive()
        .and_hms_opt(local_hour, 0, 0)
        .unwrap_or_else(|| now.naive_local());
    if t <= now.naive_local() {
        t += chrono::Duration::days(1);
    }
    t.and_utc().to_rfc3339()
}

/// Tell everybody who is meant to hear how a run went.
///
/// Two separate audiences, and the difference matters. The card goes to **you**,
/// about your own task. The outreach goes to **other people**, because the task
/// says they are to be told. Neither depends on the other: somebody who turned
/// their own notifications off has not thereby decided that their mother should
/// not hear whether the shopping was ordered.
pub async fn notify_run(state: &AppState, run_id: &str) -> anyhow::Result<()> {
    let Some(run) = errand_core::db::get_run(state.pool(), run_id).await? else {
        return Ok(());
    };
    let Some(task) = errand_core::db::get_task(state.pool(), &run.task_id).await? else {
        return Ok(());
    };

    // A run that deliberately did nothing is not news.
    if run.status == "skipped" {
        return Ok(());
    }
    let ok = run.status == "succeeded";

    let summary = run
        .summary
        .clone()
        .or_else(|| run.failure.as_ref().map(|f| f.plain_reason.clone()))
        .unwrap_or_else(|| "No details were recorded.".into());

    notify_you(state, &run, &task, ok, &summary).await?;
    notify_recipients(state, &run, &task, ok, &summary).await;
    Ok(())
}

/// The card that says how your own task went.
async fn notify_you(
    state: &AppState,
    run: &errand_core::models::Run,
    task: &errand_core::models::Task,
    ok: bool,
    summary: &str,
) -> anyhow::Result<()> {
    let notify = task.notify.clone();
    let wants = |k: &str, default: bool| notify.get(k).and_then(|v| v.as_bool()).unwrap_or(default);
    if ok && !wants("on_success", true) {
        return Ok(());
    }
    if !ok && !wants("on_failure", true) {
        return Ok(());
    }

    let Some(chat) = channels::telegram::configured_chat_id().await else {
        // Nothing configured is not an error worth failing over, but it is
        // worth saying once, because a user who thinks they set it up would
        // otherwise wonder why nothing arrives.
        tracing::info!("no Telegram chat configured, so no run notification was sent");
        return Ok(());
    };

    let duration = match (run.started_at.as_deref(), run.finished_at.as_deref()) {
        (Some(a), Some(b)) => {
            let pa = chrono::DateTime::parse_from_rfc3339(a).ok();
            let pb = chrono::DateTime::parse_from_rfc3339(b).ok();
            match (pa, pb) {
                (Some(a), Some(b)) => (b - a).num_seconds(),
                _ => 0,
            }
        }
        _ => 0,
    };

    // The Narrator's actual job. If a model is configured for it, it rewrites
    // the raw summary into something worth reading on a phone; if not, or if it
    // fails, the raw summary goes out unchanged. A notification is never held up
    // or lost over its own wording.
    let summary = narrate(state, &run.id, &task.name, ok, summary).await;

    let body = channels::telegram::result_card(&task.name, ok, &summary, duration, run.cost_usd);
    errand_core::db::enqueue_message(
        state.pool(),
        errand_core::db::NewMessage {
            run_id: Some(run.id.clone()),
            task_id: Some(run.task_id.clone()),
            class: "notify".into(),
            channel: "telegram".into(),
            recipient: chat,
            recipient_label: Some("you".into()),
            subject: None,
            body,
            is_failure: !ok,
        },
    )
    .await?;
    Ok(())
}

/// Write to the people this task was told to write to.
///
/// Everything here is deliberately quiet about failure: a contact who cannot be
/// reached must never turn a run that worked into a run that failed, so each
/// person is attempted on their own and a problem with one is logged rather
/// than raised.
async fn notify_recipients(
    state: &AppState,
    run: &errand_core::models::Run,
    task: &errand_core::models::Task,
    ok: bool,
    summary: &str,
) {
    // A rehearsal must not reach a third party. This is the same promise
    // read_brief makes to the agent, kept here rather than there, because a
    // promise enforced only in a prompt is not a promise.
    if run.mode == "dry_run" {
        return;
    }

    let people = match errand_core::db::recipients_for_task(state.pool(), &run.task_id).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(run = %run.id, "could not read who this task may write to: {e}");
            return;
        }
    };

    for person in people {
        // Whose news this is. Somebody added for failures only does not want to
        // hear that the shopping arrived.
        let wanted = if ok {
            person.on_success
        } else {
            person.on_failure
        };
        if !wanted {
            continue;
        }

        // Armed before the message is written, so a retried run does not spend a
        // model call on something it is about to be refused for.
        let fence = match errand_core::db::arm_side_effect(
            state.pool(),
            &run.id,
            &run.task_id,
            &run.occurrence_id,
            "message",
            &person.id,
        )
        .await
        {
            Ok(errand_core::db::FenceVerdict::Armed(id)) => id,
            Ok(errand_core::db::FenceVerdict::AlreadyCommitted { .. }) => {
                // Already told, most likely by the agent itself during the run.
                tracing::info!(
                    run = %run.id,
                    "{} has already been messaged for this occurrence, so nothing was sent again",
                    person.label
                );
                continue;
            }
            Ok(errand_core::db::FenceVerdict::NeedsVerification { armed_at }) => {
                tracing::warn!(
                    run = %run.id,
                    "a message to {} started at {armed_at} was never confirmed, so nothing was \
                     sent in case it arrived",
                    person.label
                );
                continue;
            }
            Err(e) => {
                tracing::warn!(run = %run.id, "could not check the message record: {e}");
                continue;
            }
        };

        let body = outreach_body(state, run, task, ok, summary, &person.label).await;
        let subject = if person.channel == "apple_mail" {
            Some(task.name.clone())
        } else {
            None
        };

        let queued = errand_core::db::enqueue_message(
            state.pool(),
            errand_core::db::NewMessage {
                run_id: Some(run.id.clone()),
                task_id: Some(run.task_id.clone()),
                // Never "test", which would walk straight through quiet hours,
                // and never a failure, which would break through the night. This
                // is somebody else's phone at three in the morning.
                class: "outreach".into(),
                channel: person.channel.clone(),
                recipient: person.address.clone(),
                recipient_label: Some(person.label.clone()),
                subject,
                body: body.clone(),
                is_failure: false,
            },
        )
        .await;

        let deduplicated = match queued {
            Ok(Some(_)) => false,
            // Identical to something sent minutes ago, so it is on its way
            // already. Committing rather than releasing is what stops a retry
            // rewording it and sending it again.
            Ok(None) => true,
            Err(e) => {
                let _ = errand_core::db::abort_side_effect(
                    state.pool(),
                    &fence,
                    "the message could not be queued",
                )
                .await;
                tracing::warn!(run = %run.id, "could not queue a message to {}: {e}", person.label);
                continue;
            }
        };

        if !deduplicated {
            // In the timeline the person reads afterwards, so they can see the
            // exact words that went out in their name. Scrubbed on the way in
            // like everything else that lands in the journal.
            let line = state
                .redactor(&run.id)
                .scrub(&format!("Told {}: {body}", person.label));
            let _ =
                errand_core::db::append_step(state.pool(), &run.id, "message", &line, true, None)
                    .await;
        }

        let evidence = serde_json::json!({
            "action": "message",
            "recipient": person.label,
            "channel": person.channel,
            "deduplicated": deduplicated,
            "at": errand_core::now_iso(),
        });
        if let Err(e) =
            errand_core::db::commit_side_effect(state.pool(), &fence, &evidence.to_string()).await
        {
            tracing::error!(
                run = %run.id,
                "a message to {} was queued but not recorded on the fence: {e}",
                person.label
            );
        }
    }
}

/// The message another person actually receives.
///
/// Written by the narrator where there is one, and by nobody where there is
/// not. The prompt is stricter than the one for your own card: this goes to
/// somebody who did not set the task up and cannot check it, so a detail the
/// run did not establish would be a lie told in the user's name.
async fn outreach_body(
    state: &AppState,
    run: &errand_core::models::Run,
    task: &errand_core::models::Task,
    ok: bool,
    summary: &str,
    label: &str,
) -> String {
    let prompt = format!(
        "Write one or two short sentences telling {label} how this went. They are not the person \
         who set this task up; they are somebody that person asked to be kept informed.\n\n\
         Task: {}\n\
         Outcome: {}\n\
         What happened: {summary}\n\n\
         Use only what is above. Do not invent a detail, a time, a place, a price, a name or a \
         link that is not in it. Do not add a greeting, a sign-off, an emoji or a question, do \
         not promise anything, and do not ask them to do anything. Reply with the sentences and \
         nothing else.",
        task.name,
        if ok { "finished" } else { "did not finish" }
    );
    let prompt = state.redactor(&run.id).scrub(&prompt);

    let written =
        match crate::models::ask(state, errand_core::providers::Role::Narrator, &prompt).await {
            Ok(a) => a.text.trim().to_string(),
            Err(e) => {
                tracing::info!("no model wrote the message to {label}, sending a plain one: {e}");
                String::new()
            }
        };

    // The narrator writes from a summary, and a summary is written from pages by
    // strangers, so what comes back is checked the same way the agent's own
    // messages are. Anything wrong with it falls back to the plain sentence
    // rather than going unsent: the person was promised they would hear.
    let allowed: Vec<String> = task
        .allowed_domains
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let scrubbed = state.redactor(&run.id).scrub(&written);
    let usable = !scrubbed.is_empty()
        && state.redactor(&run.id).is_clean(&scrubbed)
        && crate::mcp::message_body_problem(&scrubbed, &allowed).is_none();

    let body = if usable {
        scrubbed
    } else {
        plain_outreach(&task.name, ok)
    };

    // Said out loud on every automatic message. Somebody receiving this did not
    // ask Errand for anything, and a message arriving from a person they know,
    // which that person did not write, should say so.
    format!("{body}\n\nSent automatically by Errand.")
}

/// What goes out when no model wrote anything usable. Built only from the task
/// name and the outcome, so it cannot contain a detail or a link from anywhere.
fn plain_outreach(task_name: &str, ok: bool) -> String {
    if ok {
        format!("About \"{task_name}\": it has been done.")
    } else {
        format!("About \"{task_name}\": it could not be completed this time.")
    }
}

/// Turn a bare outcome into a sentence a person would want to receive.
///
/// Deliberately conservative: it is told not to invent, and anything that comes
/// back too long or empty is discarded in favour of what the run actually said.
/// A prettier message is not worth a wrong one.
async fn narrate(state: &AppState, run_id: &str, task: &str, ok: bool, summary: &str) -> String {
    let prompt = format!(
        "Rewrite this as one or two short sentences for a phone notification.\n\n\
         Task: {task}\n\
         Outcome: {}\n\
         What happened: {summary}\n\n\
         Say only what is here. Do not invent a detail, a time, a price or a name that is not \
         above. Do not add a greeting, an emoji or a sign-off. If there is nothing to say beyond \
         the outcome, repeat it plainly. Reply with the sentences and nothing else.",
        if ok { "finished" } else { "did not finish" }
    );

    // Scrubbed like everything else that reaches a model: a summary can quote a
    // page that had a secret on it. It has to be this run's redactor, since that
    // is the only one holding the secrets this run actually resolved; the
    // redactor for the empty string knows none of them and would quietly pass
    // every one of them to the model.
    let prompt = state.redactor(run_id).scrub(&prompt);

    match crate::models::ask(state, errand_core::providers::Role::Narrator, &prompt).await {
        Ok(a) => {
            let t = a.text.trim();
            // A model that rambles, returns nothing, or writes an essay is not
            // improving on the summary, so the summary wins.
            if t.is_empty() || t.len() > summary.len() * 4 + 400 {
                summary.to_string()
            } else {
                t.to_string()
            }
        }
        Err(e) => {
            tracing::info!("no model rewrote the notification, sending it as it was: {e}");
            summary.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_then_gives_up() {
        assert_eq!(BACKOFF_S[0], 30);
        assert!(BACKOFF_S.windows(2).all(|w| w[1] > w[0]));
        // A finite ladder, so a dead channel does not retry forever.
        assert!(BACKOFF_S.len() <= 6);
    }

    #[test]
    fn a_deferred_message_wakes_after_the_quiet_period() {
        let iso = next_hour_utc(8);
        assert!(chrono::DateTime::parse_from_rfc3339(&iso).is_ok());
        assert!(chrono::DateTime::parse_from_rfc3339(&iso).unwrap() > chrono::Utc::now());
    }

    // ------------------------------------------- telling somebody else how it went --

    use crate::api::testkit;
    use serde_json::json;

    const MUMS_NUMBER: &str = "+447700900123";

    /// A task with one person to tell, and a run of it, set up through the same
    /// calls the settings screen makes.
    async fn a_task_that_tells_mum(
        api: &testkit::Api,
        mode: &str,
        on_success: bool,
        on_failure: bool,
    ) -> errand_core::models::Run {
        let task_id = testkit::a_task(
            api,
            json!({ "name": "Order the shopping", "description": "Put the usual order in." }),
        )
        .await;
        let (code, person) = api
            .post(
                "/v1/recipients",
                json!({ "label": "Mum", "channel": "whatsapp", "address": MUMS_NUMBER }),
            )
            .await;
        assert_eq!(code, 200, "saving the contact failed: {person}");
        let (code, body) = api
            .post(
                &format!("/v1/tasks/{task_id}/recipients"),
                json!({
                    "recipient_id": person["id"].as_str().expect("a contact id"),
                    "on_success": on_success,
                    "on_failure": on_failure
                }),
            )
            .await;
        assert_eq!(code, 200, "granting the task access failed: {body}");

        errand_core::db::try_create_run(
            &api.pool,
            &task_id,
            &format!("manual/{}", errand_core::new_id()),
            "manual",
            mode,
            None,
        )
        .await
        .expect("a run")
    }

    /// Everything queued for somebody other than the owner.
    async fn to_other_people(api: &testkit::Api) -> Vec<errand_core::db::OutboxRow> {
        errand_core::db::due_outbox(&api.pool, 50)
            .await
            .expect("the outbox")
            .into_iter()
            .filter(|r| r.class == "outreach")
            .collect()
    }

    #[tokio::test]
    async fn a_finished_run_writes_to_the_person_the_task_was_told_to_write_to() {
        let api = testkit::start().await;
        let run = a_task_that_tells_mum(&api, "normal", true, true).await;
        errand_core::db::finish_run_ok(&api.pool, &run.id, "The usual order is booked for Friday.")
            .await
            .expect("finishing the run");

        notify_run(&api.state, &run.id)
            .await
            .expect("queueing the reports");

        let sent = to_other_people(&api).await;
        assert_eq!(
            sent.len(),
            1,
            "nobody was told the job was done: {sent:?}. This is the whole point of adding a \
             contact to a task."
        );
        assert_eq!(
            sent[0].channel, "whatsapp",
            "the channel must come from the stored contact and nowhere else"
        );
        assert_eq!(sent[0].recipient, MUMS_NUMBER);
        assert!(
            sent[0].body.contains("Sent automatically by Errand"),
            "somebody receiving this did not ask Errand for anything, so it has to say what it \
             is: {}",
            sent[0].body
        );
    }

    #[tokio::test]
    async fn bad_news_for_somebody_else_still_waits_until_a_civilised_hour() {
        // A failure you asked to hear about breaks through quiet hours, because
        // being told at nine that the eight o'clock booking failed is too late.
        // None of that reasoning applies to somebody else's phone at 03:00.
        let api = testkit::start().await;
        let run = a_task_that_tells_mum(&api, "normal", true, true).await;
        errand_core::db::finish_run_failed(
            &api.pool,
            &run.id,
            "target_unavailable",
            "The shop's site was down all morning.",
            None,
        )
        .await
        .expect("finishing the run");

        notify_run(&api.state, &run.id)
            .await
            .expect("queueing the reports");

        let sent = to_other_people(&api).await;
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert!(
            !sent[0].is_failure,
            "a message to a third party must never be marked as news that breaks through the night"
        );
        assert_ne!(
            sent[0].class, "test",
            "the test class skips quiet hours entirely, and this is somebody's phone"
        );
    }

    #[tokio::test]
    async fn somebody_who_only_wanted_the_bad_news_is_not_told_the_good() {
        let api = testkit::start().await;
        let run = a_task_that_tells_mum(&api, "normal", false, true).await;
        errand_core::db::finish_run_ok(&api.pool, &run.id, "Ordered.")
            .await
            .expect("finishing the run");

        notify_run(&api.state, &run.id)
            .await
            .expect("queueing the reports");

        assert!(
            to_other_people(&api).await.is_empty(),
            "somebody who asked only to hear about problems was told about a success"
        );
    }

    #[tokio::test]
    async fn a_rehearsal_reaches_nobody_at_all() {
        let api = testkit::start().await;
        let run = a_task_that_tells_mum(&api, "dry_run", true, true).await;
        errand_core::db::finish_run_ok(&api.pool, &run.id, "Would have ordered the usual.")
            .await
            .expect("finishing the run");

        notify_run(&api.state, &run.id)
            .await
            .expect("queueing the reports");

        assert!(
            to_other_people(&api).await.is_empty(),
            "a rehearsal messaged a real person"
        );
    }

    #[tokio::test]
    async fn a_second_report_of_the_same_run_does_not_tell_anybody_twice() {
        // A retried notification, or two runners racing over one finished run.
        let api = testkit::start().await;
        let run = a_task_that_tells_mum(&api, "normal", true, true).await;
        errand_core::db::finish_run_ok(&api.pool, &run.id, "The usual order is booked.")
            .await
            .expect("finishing the run");

        notify_run(&api.state, &run.id).await.expect("first report");
        notify_run(&api.state, &run.id)
            .await
            .expect("second report");

        assert_eq!(
            to_other_people(&api).await.len(),
            1,
            "the same person was written to twice about one run"
        );
    }

    #[tokio::test]
    async fn what_reaches_the_model_is_scrubbed_with_this_run_s_own_secrets() {
        // The bug this replaced: the notifier scrubbed with the redactor for the
        // empty string, which knows none of the secrets the run actually
        // resolved, so a summary quoting one handed it straight to the model.
        let api = testkit::start().await;
        let run = a_task_that_tells_mum(&api, "normal", true, true).await;
        api.state
            .redactor(&run.id)
            .register("hunter2horse", "Shop password");

        let scrubbed = api
            .state
            .redactor(&run.id)
            .scrub("the page still said hunter2horse");
        assert!(
            !scrubbed.contains("hunter2horse"),
            "the run's redactor is the only one that knows this run's secrets: {scrubbed}"
        );
        assert!(
            api.state
                .redactor("")
                .scrub("the page still said hunter2horse")
                .contains("hunter2horse"),
            "if the empty-string redactor knew it, this test would prove nothing"
        );
    }
}

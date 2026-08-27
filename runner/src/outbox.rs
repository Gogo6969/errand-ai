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
            // Unconditional. There is always a quiet period, because a fresh
            // install has nobody's saved preference in it and a run finishing at
            // 03:00 would otherwise wake a third party who never asked Errand
            // for anything.
            let (from, to, breaks) = quiet_hours(state).await;
            if channels::deferred_until(hour, from, to, row.is_failure, breaks) {
                errand_core::db::defer_outbox(state.pool(), &row.id, next_hour_utc(to)).await?;
                continue;
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

/// The row the settings screen writes, and the only one read here.
pub const QUIET_SETTING: &str = "messaging.quiet";

/// The night, as it stands before anybody has chosen one: 22:00 to 07:00, with
/// a failure you asked to hear about still reaching you during it.
///
/// Absent is not the same as "no quiet hours". Nobody installs Errand meaning
/// "message my mother at three in the morning", so an install where the setting
/// has never been touched gets an ordinary night rather than none.
const DEFAULT_QUIET: (u32, u32, bool) = (22, 7, true);

/// The default written in the exact shape the settings table stores, so
/// `GET /v1/settings` can hand back the period that is genuinely in force when
/// no row exists.
///
/// This exists so there is one set of numbers rather than two. A screen showing
/// 22 and 7 while the outbox defers nothing is worse than a blank screen: it
/// tells a worried person their evenings are protected when they are not.
pub fn default_quiet_hours() -> serde_json::Value {
    let (from, to, breaks) = DEFAULT_QUIET;
    serde_json::json!({ "from": from, "to": to, "failure_breaks_through": breaks })
}

/// The quiet period in force, whether or not anybody has saved one.
async fn quiet_hours(state: &AppState) -> (u32, u32, bool) {
    let stored = errand_core::db::get_setting(state.pool(), QUIET_SETTING)
        .await
        .ok()
        .flatten();
    // Falls back to the same JSON the settings screen is given, not to a second
    // copy of the numbers, so the two cannot drift apart.
    let v = stored.unwrap_or_else(default_quiet_hours);
    parse_quiet(&v).unwrap_or(DEFAULT_QUIET)
}

/// Read a stored quiet period, or nothing if it is not the shape this reads.
///
/// A half-written row is treated as no row at all rather than as half a night:
/// a missing "to" hour must not become an open-ended quiet period, and it must
/// not become an absent one either.
fn parse_quiet(v: &serde_json::Value) -> Option<(u32, u32, bool)> {
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

/// The next time it is this hour where the person actually is, as an instant.
///
/// The conversion at the end is the whole job and it used to be wrong: a naive
/// local time was stamped as UTC rather than converted, so a message held until
/// seven in the morning went out at nine in Vienna and the evening before at
/// UTC-11. Quiet hours exist precisely so a message does not arrive at a rude
/// hour, and a timezone slip here delivers the one thing they prevent.
///
/// Daylight saving is handled the way core/src/schedule.rs handles it, because
/// two answers to "when is 02:30" in one program is worse than either answer:
/// a local time that never happened runs just after the gap, and an ambiguous
/// one takes the first occurrence.
fn next_hour_utc(local_hour: u32) -> String {
    use chrono::{TimeZone, Timelike};

    let now = chrono::Local::now();
    let mut day = now.date_naive();
    if now.hour() >= local_hour {
        day += chrono::Duration::days(1);
    }

    for extra in 0..3 {
        let wanted = (day + chrono::Duration::days(extra))
            .and_hms_opt(local_hour, 0, 0)
            .expect("an hour of the day is always a valid time");
        match chrono::Local.from_local_datetime(&wanted) {
            // The ordinary case, and the ambiguous one: earliest() takes the
            // first occurrence when the clocks have just gone back.
            chrono::LocalResult::Single(t) => return t.with_timezone(&chrono::Utc).to_rfc3339(),
            chrono::LocalResult::Ambiguous(t, _) => {
                return t.with_timezone(&chrono::Utc).to_rfc3339()
            }
            // An hour that never happened, because the clocks went forward
            // through it. Wait for the first moment that did.
            chrono::LocalResult::None => {
                let after = wanted + chrono::Duration::hours(1);
                if let Some(t) = chrono::Local.from_local_datetime(&after).earliest() {
                    return t.with_timezone(&chrono::Utc).to_rfc3339();
                }
            }
        }
    }

    // Unreachable in practice: three days without that hour existing is not a
    // timezone, it is a bug. An hour from now is a safe thing to do about it.
    (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339()
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

    // The answer, because that is the thing the task exists to produce. This
    // used to send the summary, which the finish tool spends eleven lines
    // telling the agent is explicitly NOT the answer: "One line, past tense,
    // about the work itself. Not the answer, and not a repeat of it." So a
    // scheduled task woke somebody's phone at seven to tell them where it had
    // been, while what it found stayed in the database.
    let summary = run
        .answer
        .clone()
        .filter(|a| !a.trim().is_empty())
        .or_else(|| run.summary.clone())
        .or_else(|| run.failure.as_ref().map(|f| f.plain_reason.clone()))
        .unwrap_or_else(|| "No details were recorded.".into());

    notify_you(state, &run, &task, ok, &summary).await?;
    notify_recipients(state, &run, &task, ok, &summary).await;
    Ok(())
}

/// Where to reach the person who set the task up.
///
/// Telegram first only because it was here first and somebody may already rely
/// on it. Otherwise, whichever way of reaching them is set up. This used to be
/// Telegram or nothing, which meant a person who does not use Telegram, or
/// lives where it is blocked, installed a scheduler that ran every morning and
/// never told them anything: they had filled in their own address on another
/// channel, watched a test message arrive, and then heard nothing ever again.
async fn your_address(state: &AppState) -> Option<(String, String)> {
    if let Some(chat) = channels::telegram::configured_chat_id().await {
        return Some(("telegram".into(), chat));
    }
    for channel in ["imessage", "apple_mail", "whatsapp"] {
        let key = format!("messaging.self.{channel}");
        if let Ok(Some(v)) = errand_core::db::get_setting(state.pool(), &key).await {
            if let Some(addr) = v.as_str().map(str::trim).filter(|a| !a.is_empty()) {
                return Some((channel.to_string(), addr.to_string()));
            }
        }
    }
    None
}

/// The same card, for a channel that carries words rather than markup.
fn plain_card(task: &str, ok: bool, body: &str) -> String {
    let head = if ok { task.to_string() } else { format!("{task}: could not finish") };
    format!("{head}\n\n{body}")
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

    let Some((channel, address)) = your_address(state).await else {
        tracing::info!(
            "no way of reaching you is set up, so no run notification was sent; \
             Settings has a place for one on each channel"
        );
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

    // Telegram takes an HTML card; everything else takes what a person would
    // write. One shape per channel rather than one shape everywhere, because a
    // card full of tags arriving as a text message is worse than plain words.
    let body = if channel == "telegram" {
        channels::telegram::result_card(&task.name, ok, &summary, duration, run.cost_usd)
    } else {
        plain_card(&task.name, ok, &summary)
    };
    errand_core::db::enqueue_message(
        state.pool(),
        errand_core::db::NewMessage {
            run_id: Some(run.id.clone()),
            task_id: Some(run.task_id.clone()),
            class: "notify".into(),
            channel: channel.clone(),
            recipient: address,
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
    if run.is_rehearsal() {
        return;
    }

    let people = match errand_core::db::recipients_for_task(state.pool(), &run.task_id).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(run = %run.id, "could not read who this task may write to: {e}");
            return;
        }
    };

    // The ceiling the agent's own message_person enforces, applied here too and
    // counted from the same journal, so the agent's sends and this report share
    // one budget. Without it a task allowed one message sends one during the run
    // and another twenty from here, one to every person it happens to be linked
    // to, and the number on the task page means nothing.
    let limits = errand_core::limits::Limits::from_json(&task.limits);
    let mut sent = crate::mcp::messages_this_run(state, &run.id).await;

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

        // Checked before the fence, never after. Arming first would spend this
        // occurrence's one message to somebody who is then never told, and the
        // next run would be refused for a message that never happened.
        if limits.max_messages > 0 && sent >= limits.max_messages {
            // Written down rather than only logged, and deliberately not as a
            // step of kind "message": the count above is made of those, so
            // recording a refusal as one would spend budget on refusing. Somebody
            // who set the ceiling below the number of people they linked would
            // otherwise never find out that their sister was not told.
            let line = format!(
                "Did not tell {}: this run has already sent {sent} message{}, which is all this \
                 task allows.",
                person.label,
                if sent == 1 { "" } else { "s" }
            );
            let _ =
                errand_core::db::append_step(state.pool(), &run.id, "decide", &line, false, None)
                    .await;
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
            Ok(errand_core::db::FenceVerdict::Armed(id)) => {
                // The same follow-up check message_person makes, asked here
                // before the narrator is paid to write a word. The fence guards a
                // scheduled slot, but pressing Run now twice mints a fresh slot
                // each time, so without this the same person hears the same news
                // twice in a minute.
                match crate::mcp::messaged_moments_ago(
                    state,
                    &run.task_id,
                    &person.id,
                    &run.occurrence_id,
                )
                .await
                {
                    Ok(None) => id,
                    Ok(Some((at, _))) => {
                        let _ = errand_core::db::abort_side_effect(
                            state.pool(),
                            &id,
                            "this person had just been messaged",
                        )
                        .await;
                        let line = format!(
                            "Did not tell {} again: they were written to at {at}, only minutes \
                             ago.",
                            person.label
                        );
                        let _ = errand_core::db::append_step(
                            state.pool(),
                            &run.id,
                            "decide",
                            &line,
                            false,
                            None,
                        )
                        .await;
                        tracing::info!(
                            run = %run.id,
                            "{} was messaged at {at}, only minutes ago, so nothing was sent again",
                            person.label
                        );
                        continue;
                    }
                    Err(e) => {
                        let _ = errand_core::db::abort_side_effect(
                            state.pool(),
                            &id,
                            "the record of what had already been sent could not be read",
                        )
                        .await;
                        tracing::warn!(
                            run = %run.id,
                            "could not check whether {} had just been written to, so nothing was \
                             sent: {e}",
                            person.label
                        );
                        continue;
                    }
                }
            }
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
            // Counted here, against the step just written, so the running total
            // and the journal the ceiling is read from can never disagree.
            sent += 1;
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
    use errand_core::models::RunMode;

    #[test]
    fn a_message_held_until_morning_really_goes_out_in_the_morning() {
        // This caught a real bug: a local time was stamped as UTC rather than
        // converted, so a message held until seven went out at nine in Vienna
        // and the evening before at UTC-11. Quiet hours exist so a message does
        // not arrive at a rude hour; getting the conversion wrong delivers the
        // one thing they prevent. Asserted in local terms, since that is what
        // the person experiences, and so it holds in any timezone the test runs
        // in rather than only the one this machine happens to be set to.
        use chrono::Timelike;

        for hour in [0u32, 7, 13, 23] {
            let iso = next_hour_utc(hour);
            let at = chrono::DateTime::parse_from_rfc3339(&iso)
                .expect("a real instant")
                .with_timezone(&chrono::Local);

            // Round-tripped back to the clock on the wall, it is the hour asked
            // for. Away from a daylight-saving jump the minute is zero too; a
            // zone that shifts by half an hour can legitimately land elsewhere.
            assert_eq!(
                at.hour(),
                hour,
                "asked for {hour} o'clock, got {at} (from {iso})"
            );
            assert!(
                at > chrono::Local::now(),
                "a message must wait for the next {hour} o'clock, not one that has gone: {at}"
            );
            assert!(
                at < chrono::Local::now() + chrono::Duration::hours(25),
                "the next {hour} o'clock is always within a day: {at}"
            );
        }
    }

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

    // ----------------------------------------------- the night nobody set up --

    #[tokio::test]
    async fn what_reaches_your_phone_is_the_answer_and_not_the_story_of_the_work() {
        // finish spends eleven lines telling the agent that summary is
        // explicitly not the answer. This used to send the summary, so a task
        // that woke somebody at seven told them where it had been while what
        // it found stayed in the database.
        let api = crate::api::testkit::start().await;
        let task_id = crate::api::testkit::a_ready_manual_task(&api).await;
        let run = errand_core::db::create_run(
            &api.pool,
            &task_id,
            "occ-notify",
            "schedule",
            errand_core::models::RunMode::NORMAL,
            None,
        )
        .await
        .expect("a run");
        errand_core::db::finish_run_ok(
            &api.pool,
            &run.id,
            "Went to the site and read the table.",
            Some("Gold is 4,592.90 dollars an ounce, down 1.4 percent today."),
        )
        .await
        .expect("finishing");
        errand_core::db::set_setting(
            &api.pool,
            "messaging.self.imessage",
            &serde_json::json!("+1 555 0123"),
        )
        .await
        .expect("an address to reach you on");

        notify_run(&api.state, &run.id).await.expect("notifying");

        let queued = errand_core::db::due_outbox(&api.pool, 10)
            .await
            .expect("the outbox");
        let mine: Vec<_> = queued.iter().filter(|m| m.class == "notify").collect();
        assert_eq!(mine.len(), 1, "{queued:?}");
        assert!(
            mine[0].body.contains("4,592.90"),
            "the answer never left the database: {}",
            mine[0].body
        );
        assert!(
            !mine[0].body.contains("read the table"),
            "it sent the story of the work instead: {}",
            mine[0].body
        );
        assert_eq!(
            mine[0].channel, "imessage",
            "it only knew how to reach you on Telegram"
        );
    }

    #[tokio::test]
    async fn a_brand_new_install_already_has_a_quiet_period_nobody_had_to_switch_on() {
        let api = testkit::start().await;
        assert!(
            errand_core::db::get_setting(&api.pool, QUIET_SETTING)
                .await
                .expect("reading the settings")
                .is_none(),
            "this test is only meaningful while nothing has been saved, which is the state \
             every new install is in"
        );

        let (from, to, breaks) = quiet_hours(&api.state).await;
        assert_eq!(
            (from, to),
            (22, 7),
            "with no quiet period a run finishing at three in the morning writes to somebody's \
             mother there and then"
        );
        assert!(
            breaks,
            "a failure you asked to hear about should still reach you during the night"
        );
        assert!(
            channels::deferred_until(3, from, to, false, breaks),
            "good news at 03:00 has to wait until morning"
        );
        assert!(
            !channels::deferred_until(14, from, to, false, breaks),
            "the afternoon is not the middle of the night"
        );
    }

    #[tokio::test]
    async fn the_quiet_hours_the_settings_screen_is_shown_are_the_ones_the_outbox_applies() {
        // The screen reads GET /v1/settings, which is handed
        // `default_quiet_hours()`. The outbox reads `quiet_hours`. Written out
        // twice, the two would drift, and a person would be shown 22 and 7 on a
        // screen while nothing whatever was being held back.
        let api = testkit::start().await;
        errand_core::db::set_setting(&api.pool, QUIET_SETTING, &default_quiet_hours())
            .await
            .expect("saving the default the screen shows");
        assert_eq!(
            quiet_hours(&api.state).await,
            (22, 7, true),
            "the shape the screen is given must be the shape the outbox reads back"
        );
    }

    #[tokio::test]
    async fn quiet_hours_saved_with_a_piece_missing_still_leave_a_night_in_place() {
        let api = testkit::start().await;
        errand_core::db::set_setting(&api.pool, QUIET_SETTING, &json!({ "from": 23 }))
            .await
            .expect("saving a half-written row");
        assert_eq!(
            quiet_hours(&api.state).await,
            (22, 7, true),
            "half a setting must not become half a night, and must not become no night at all"
        );
    }

    #[tokio::test]
    async fn somebody_who_deliberately_turned_quiet_hours_off_still_has_them_off() {
        // The default fills a gap. It must never overrule a decision somebody
        // actually made, or the switch on the settings screen does nothing.
        let api = testkit::start().await;
        errand_core::db::set_setting(
            &api.pool,
            QUIET_SETTING,
            &json!({ "from": 0, "to": 0, "failure_breaks_through": false }),
        )
        .await
        .expect("saving the choice");
        let (from, to, breaks) = quiet_hours(&api.state).await;
        assert_eq!((from, to, breaks), (0, 0, false));
        assert!(
            !channels::deferred_until(3, from, to, false, breaks),
            "somebody who switched the night off was given one back"
        );
    }

    // ------------------------------------------- telling somebody else how it went --

    use crate::api::testkit;
    use serde_json::json;

    const MUMS_NUMBER: &str = "+447700900123";

    /// A task with one person to tell, and a run of it, set up through the same
    /// calls the settings screen makes.
    async fn a_task_that_tells_mum(
        api: &testkit::Api,
        mode: RunMode,
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
        let run = a_task_that_tells_mum(&api, RunMode::NORMAL, true, true).await;
        errand_core::db::finish_run_ok(&api.pool, &run.id, "The usual order is booked for Friday.", None)
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
        let run = a_task_that_tells_mum(&api, RunMode::NORMAL, true, true).await;
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
        let run = a_task_that_tells_mum(&api, RunMode::NORMAL, false, true).await;
        errand_core::db::finish_run_ok(&api.pool, &run.id, "Ordered.", None)
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
        let run = a_task_that_tells_mum(&api, RunMode::REHEARSAL, true, true).await;
        errand_core::db::finish_run_ok(&api.pool, &run.id, "Would have ordered the usual.", None)
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
        let run = a_task_that_tells_mum(&api, RunMode::NORMAL, true, true).await;
        errand_core::db::finish_run_ok(&api.pool, &run.id, "The usual order is booked.", None)
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

    // -------------------------------- the ceiling, and telling somebody twice --

    const DADS_NUMBER: &str = "+447700900124";
    const SISTERS_NUMBER: &str = "+447700900125";

    /// A task with a stated message budget and a list of people to tell, all set
    /// up through the calls the app itself makes.
    async fn a_task_that_tells(
        api: &testkit::Api,
        max_messages: i64,
        people: &[(&str, &str)],
    ) -> String {
        let task_id = testkit::a_task(
            api,
            json!({
                "name": "Order the shopping",
                "description": "Put the usual order in.",
                "limits": { "max_messages": max_messages }
            }),
        )
        .await;
        for (label, address) in people {
            let (code, person) = api
                .post(
                    "/v1/recipients",
                    json!({ "label": label, "channel": "whatsapp", "address": address }),
                )
                .await;
            assert_eq!(code, 200, "saving the contact failed: {person}");
            let (code, body) = api
                .post(
                    &format!("/v1/tasks/{task_id}/recipients"),
                    json!({
                        "recipient_id": person["id"].as_str().expect("a contact id"),
                        "on_success": true,
                        "on_failure": true
                    }),
                )
                .await;
            assert_eq!(code, 200, "granting the task access failed: {body}");
        }
        task_id
    }

    /// One press of Run now. Each press is its own occurrence, which is the
    /// whole reason the follow-up check below has to exist.
    async fn a_press_of_run_now(api: &testkit::Api, task_id: &str) -> errand_core::models::Run {
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
    }

    /// Everything written into this run's timeline.
    async fn timeline(api: &testkit::Api, run_id: &str) -> Vec<errand_core::models::Step> {
        errand_core::db::list_steps(&api.pool, run_id)
            .await
            .expect("the timeline")
    }

    #[tokio::test]
    async fn a_task_allowed_one_message_does_not_write_to_three_people_when_the_run_ends() {
        let api = testkit::start().await;
        let task_id = a_task_that_tells(
            &api,
            1,
            &[
                ("Mum", MUMS_NUMBER),
                ("Dad", DADS_NUMBER),
                ("Sister", SISTERS_NUMBER),
            ],
        )
        .await;
        let run = a_press_of_run_now(&api, &task_id).await;
        errand_core::db::finish_run_ok(&api.pool, &run.id, "The usual order is booked for Friday.", None)
            .await
            .expect("finishing the run");

        notify_run(&api.state, &run.id)
            .await
            .expect("queueing the reports");

        let sent = to_other_people(&api).await;
        assert_eq!(
            sent.len(),
            1,
            "the task says one message and {} went out. A ceiling the automatic report ignores \
             is not a ceiling: {sent:?}",
            sent.len()
        );

        // Whoever was left out is named, so somebody who set the number lower
        // than the number of people they linked finds out.
        let steps = timeline(&api, &run.id).await;
        let skipped: Vec<&str> = steps
            .iter()
            .filter(|s| s.title.starts_with("Did not tell"))
            .map(|s| s.title.as_str())
            .collect();
        assert_eq!(
            skipped.len(),
            2,
            "the two people who were not told are not mentioned anywhere: {steps:?}"
        );
        assert!(
            skipped
                .iter()
                .all(|t| t.contains("which is all this task allows")),
            "the timeline has to say why, not just that: {skipped:?}"
        );
        assert!(
            !steps
                .iter()
                .any(|s| s.kind == "message" && s.title.starts_with("Did not tell")),
            "a refusal recorded as a message would be counted as one, and refusing would spend \
             the very budget it is enforcing"
        );
    }

    #[tokio::test]
    async fn the_messages_the_agent_sent_itself_count_against_the_report_at_the_end() {
        // One budget, not two. If the agent spends the task's only message
        // during the run, there is none left for the automatic report.
        let api = testkit::start().await;
        let task_id = a_task_that_tells(&api, 1, &[("Mum", MUMS_NUMBER)]).await;
        let run = a_press_of_run_now(&api, &task_id).await;
        errand_core::db::append_step(
            &api.pool,
            &run.id,
            "message",
            "Messaged Mum on WhatsApp: the order is in.",
            true,
            None,
        )
        .await
        .expect("the agent's own message");
        errand_core::db::finish_run_ok(&api.pool, &run.id, "Ordered.", None)
            .await
            .expect("finishing the run");

        notify_run(&api.state, &run.id)
            .await
            .expect("queueing the reports");

        assert!(
            to_other_people(&api).await.is_empty(),
            "the run had already used the task's one message, and it sent another anyway"
        );
    }

    #[tokio::test]
    async fn pressing_run_now_twice_does_not_tell_the_same_person_twice_over() {
        let api = testkit::start().await;
        let task_id = a_task_that_tells(&api, 3, &[("Mum", MUMS_NUMBER)]).await;

        let first = a_press_of_run_now(&api, &task_id).await;
        errand_core::db::finish_run_ok(&api.pool, &first.id, "The usual order is booked.", None)
            .await
            .expect("finishing the first run");
        notify_run(&api.state, &first.id)
            .await
            .expect("first report");

        // A second press mints a fresh occurrence, so the fence on its own does
        // not cover it. The outcome differs deliberately, so the words differ
        // too and the outbox's ten-minute duplicate check cannot be what saves
        // her from hearing it again.
        let second = a_press_of_run_now(&api, &task_id).await;
        errand_core::db::finish_run_failed(
            &api.pool,
            &second.id,
            "target_unavailable",
            "The shop's site was down.",
            None,
        )
        .await
        .expect("finishing the second run");
        notify_run(&api.state, &second.id)
            .await
            .expect("second report");

        let sent = to_other_people(&api).await;
        assert_eq!(
            sent.len(),
            1,
            "Mum was written to twice within minutes about the same task: {sent:?}"
        );
        assert!(
            timeline(&api, &second.id)
                .await
                .iter()
                .any(|s| s.title.contains("Did not tell Mum again")),
            "the second run has to say it held the message back, and that it was the follow-up \
             check that did it"
        );
    }

    #[tokio::test]
    async fn two_different_people_are_both_told_about_the_same_run() {
        // The follow-up check is asked about one person at a time. If it were
        // not, writing to Mum would silence Dad, and somebody who asked to be
        // kept informed would simply never hear anything.
        let api = testkit::start().await;
        let task_id =
            a_task_that_tells(&api, 3, &[("Mum", MUMS_NUMBER), ("Dad", DADS_NUMBER)]).await;
        let run = a_press_of_run_now(&api, &task_id).await;
        errand_core::db::finish_run_ok(&api.pool, &run.id, "The usual order is booked.", None)
            .await
            .expect("finishing the run");

        notify_run(&api.state, &run.id)
            .await
            .expect("queueing the reports");

        let sent = to_other_people(&api).await;
        assert_eq!(
            sent.len(),
            2,
            "one of the two people this task was told to write to heard nothing: {sent:?}"
        );
    }

    #[tokio::test]
    async fn what_reaches_the_model_is_scrubbed_with_this_run_s_own_secrets() {
        // The bug this replaced: the notifier scrubbed with the redactor for the
        // empty string, which knows none of the secrets the run actually
        // resolved, so a summary quoting one handed it straight to the model.
        let api = testkit::start().await;
        let run = a_task_that_tells_mum(&api, RunMode::NORMAL, true, true).await;
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

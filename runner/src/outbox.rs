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

/// Queue the card that says how a run went.
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

    let summary = run
        .summary
        .clone()
        .or_else(|| run.failure.as_ref().map(|f| f.plain_reason.clone()))
        .unwrap_or_else(|| "No details were recorded.".into());

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
    let summary = narrate(state, &task.name, ok, &summary).await;

    let body = channels::telegram::result_card(&task.name, ok, &summary, duration, run.cost_usd);
    errand_core::db::enqueue_message(
        state.pool(),
        errand_core::db::NewMessage {
            run_id: Some(run_id.to_string()),
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

/// Turn a bare outcome into a sentence a person would want to receive.
///
/// Deliberately conservative: it is told not to invent, and anything that comes
/// back too long or empty is discarded in favour of what the run actually said.
/// A prettier message is not worth a wrong one.
async fn narrate(state: &AppState, task: &str, ok: bool, summary: &str) -> String {
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
    // page that had a secret on it.
    let prompt = state.redactor("").scrub(&prompt);

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
}

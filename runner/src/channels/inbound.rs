//! Asking Errand things from your phone.
//!
//! Read-only on purpose. A Telegram bot is addressable by anyone who discovers
//! its username, and the token protects sending, not receiving. So every update
//! is checked against one configured owner and dropped otherwise, and even for
//! the owner the commands here only look at things. Nothing on this surface
//! starts a run, approves a booking, or spends money, because a chat message is
//! not a good place to authorise something irreversible.

use crate::state::AppState;

const POLL_TIMEOUT_S: u64 = 30;

pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let mut offset: i64 = 0;
        loop {
            // Nothing to poll until a bot is configured, so this costs nothing
            // for the many people who never set one up.
            if super::telegram::owner_user_id().await.is_none() {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                continue;
            }
            match poll(&state, offset).await {
                Ok(Some(next)) => offset = next,
                Ok(None) => {}
                Err(e) => {
                    tracing::debug!("telegram poll failed: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                }
            }
        }
    });
}

async fn poll(state: &AppState, offset: i64) -> anyhow::Result<Option<i64>> {
    let Some(updates) = super::telegram::get_updates(offset, POLL_TIMEOUT_S).await? else {
        return Ok(None);
    };
    let owner = super::telegram::owner_user_id().await;
    let mut next = offset;

    for u in updates {
        next = u.update_id + 1;

        // The whole security of this surface. Anything not from the owner is
        // dropped without a reply, so a stranger who finds the bot learns
        // nothing and gets nothing.
        if !super::telegram::is_owner(&u.from_user_id, owner.as_deref()) {
            tracing::warn!(
                from = %u.from_user_id,
                "ignoring a Telegram message from someone who is not the owner"
            );
            continue;
        }

        let reply = handle(state, &u.text).await;
        if let Err(e) = super::telegram::send(&u.chat_id, &reply).await {
            tracing::warn!("could not reply on Telegram: {e}");
        }
    }
    Ok(Some(next))
}

/// The command word, however it was typed.
pub fn parse_command(text: &str) -> String {
    text.split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .trim_start_matches('/')
        // Telegram appends the bot name when several bots share a group.
        .split('@')
        .next()
        .unwrap_or("")
        .to_string()
}

/// Answer one command.
pub async fn handle(state: &AppState, text: &str) -> String {
    match parse_command(text).as_str() {
        "status" | "" => status(state).await,
        "runs" => recent_runs(state).await,
        "tasks" => tasks(state).await,
        "help" => help(),
        other => format!("I do not know '{other}'.\n\n{}", help()),
    }
}

fn help() -> String {
    "What I can tell you:\n\
     /status  how things are right now\n\
     /tasks   your tasks and when each runs next\n\
     /runs    how the last few runs went\n\n\
     I only answer questions here. Starting a run, approving something, or changing a task all \
     happen in Errand itself, because a chat message is not a good place to authorise something \
     that cannot be undone."
        .to_string()
}

async fn status(state: &AppState) -> String {
    let busy = errand_core::db::count_busy_runs(state.pool())
        .await
        .unwrap_or(0);
    let tasks = errand_core::db::list_tasks(state.pool(), false)
        .await
        .unwrap_or_default();
    let ready = tasks.iter().filter(|t| t.status == "ready").count();
    let paused = tasks.iter().filter(|t| t.status == "paused").count();

    let next = tasks
        .iter()
        .filter_map(|t| t.next_run_at.as_ref().map(|n| (n.clone(), t.name.clone())))
        .min();

    let mut out =
        format!("Errand is running.\n{ready} task(s) armed, {paused} paused, {busy} running now.");
    if let Some((when, name)) = next {
        out.push_str(&format!("\n\nNext up: {name} at {when}"));
    }
    if paused > 0 {
        out.push_str("\n\nSomething paused usually means it needs you. Check it in Errand.");
    }
    out
}

async fn tasks(state: &AppState) -> String {
    let tasks = errand_core::db::list_tasks(state.pool(), false)
        .await
        .unwrap_or_default();
    if tasks.is_empty() {
        return "You have no tasks yet.".into();
    }
    let mut out = String::from("Your tasks:\n");
    for t in tasks.iter().take(15) {
        out.push_str(&format!(
            "\n{} {} [{}]",
            t.emoji.clone().unwrap_or_else(|| "-".into()),
            t.name,
            t.status
        ));
        if let Some(n) = &t.next_run_at {
            out.push_str(&format!("\n   next: {n}"));
        }
        if t.auto_paused {
            out.push_str(&format!(
                "\n   paused by Errand: {}",
                t.paused_reason
                    .clone()
                    .unwrap_or_else(|| "needs you".into())
            ));
        }
    }
    out
}

async fn recent_runs(state: &AppState) -> String {
    let runs = errand_core::db::list_runs(state.pool(), None, 8)
        .await
        .unwrap_or_default();
    if runs.is_empty() {
        return "Nothing has run yet.".into();
    }
    let mut out = String::from("Recent runs:\n");
    for r in runs {
        let mark = match r.status.as_str() {
            "succeeded" => "ok",
            "failed" => "failed",
            "skipped" => "skipped",
            other => other,
        };
        let line = r
            .summary
            .clone()
            .or_else(|| r.failure.as_ref().map(|f| f.plain_reason.clone()))
            .unwrap_or_default();
        let first = line
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(90)
            .collect::<String>();
        out.push_str(&format!("\n[{mark}] {first}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_help_says_what_this_surface_deliberately_will_not_do() {
        let h = help();
        assert!(h.contains("only answer questions"));
        assert!(h.contains("cannot be undone"));
    }

    #[test]
    fn commands_are_recognised_however_they_are_typed() {
        assert_eq!(parse_command("/status"), "status");
        assert_eq!(parse_command("status"), "status");
        assert_eq!(parse_command("  /STATUS  "), "status");
        // Telegram appends the bot name in a group chat.
        assert_eq!(parse_command("/runs@errand_bot"), "runs");
        assert_eq!(parse_command("/tasks please"), "tasks");
        assert_eq!(parse_command(""), "");
    }

    #[test]
    fn an_unknown_command_is_not_mistaken_for_a_known_one() {
        assert_eq!(parse_command("/launch_missiles"), "launch_missiles");
        assert_ne!(parse_command("/statuses"), "status");
    }
}

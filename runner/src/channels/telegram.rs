//! Telegram, the channel that tells you what happened.
//!
//! This is the guaranteed path. WhatsApp rides an unofficial gateway that
//! decays, and the Apple channels need consent that can be revoked, but a bot
//! token either works or says why. So run outcomes go here.
//!
//! The part that matters for safety is inbound rather than outbound. A bot is
//! addressable by anyone who knows its username, and the buttons on a result
//! card approve real bookings. Every incoming update is checked against one
//! configured owner and dropped otherwise.

use super::{ChannelError, ChannelId, Health, SendResult};

const API: &str = "https://api.telegram.org";

/// Where the token and chat id live. Never in the database, never in config.
pub const ACCOUNT_TOKEN: &str = "telegram.bot_token";
pub const ACCOUNT_CHAT_ID: &str = "telegram.chat_id";
pub const ACCOUNT_OWNER_ID: &str = "telegram.owner_user_id";

fn base_url() -> String {
    // Overridable so the tests can point at a local stand-in rather than
    // messaging a real person.
    std::env::var("ERRAND_TELEGRAM_API").unwrap_or_else(|_| API.to_string())
}

async fn token() -> Option<String> {
    crate::secrets::get_internal(ACCOUNT_TOKEN)
        .await
        .ok()
        .map(|s| s.expose().to_string())
}

pub async fn configured_chat_id() -> Option<String> {
    crate::secrets::get_internal(ACCOUNT_CHAT_ID)
        .await
        .ok()
        .map(|s| s.expose().to_string())
}

/// The one account allowed to command this bot.
pub async fn owner_user_id() -> Option<String> {
    crate::secrets::get_internal(ACCOUNT_OWNER_ID)
        .await
        .ok()
        .map(|s| s.expose().to_string())
}

/// Is this update from the person who owns this Errand?
///
/// Without this, anyone who finds the bot can press Approve on a booking. The
/// token protects sending, not receiving.
pub fn is_owner(from_user_id: &str, owner: Option<&str>) -> bool {
    match owner {
        Some(o) => !o.is_empty() && o == from_user_id,
        // No owner configured means nobody is trusted, rather than everybody.
        None => false,
    }
}

pub async fn send(chat_id: &str, body: &str) -> SendResult {
    let Some(tok) = token().await else {
        return Err(ChannelError::NeedsUser {
            why: "Telegram has no bot token saved".into(),
            fix: "Add one in settings, then send a test message.".into(),
        });
    };

    let url = format!("{}/bot{}/sendMessage", base_url(), tok);
    let res = reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "text": body,
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
        }))
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| ChannelError::Transient(format!("could not reach Telegram: {e}")))?;

    let status = res.status();
    let text = res.text().await.unwrap_or_default();

    if status.is_success() {
        let id = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v["result"]["message_id"].as_i64())
            .map(|i| i.to_string())
            .unwrap_or_default();
        return Ok(id);
    }

    if status.as_u16() == 429 {
        let after = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v["parameters"]["retry_after"].as_u64())
            .unwrap_or(30);
        return Err(ChannelError::RateLimited(std::time::Duration::from_secs(
            after,
        )));
    }
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(ChannelError::NeedsUser {
            why: format!("Telegram refused the bot token or the chat ({status})"),
            fix: "Check the token, and send the bot a message once so it is allowed to reply."
                .into(),
        });
    }
    if status.is_client_error() {
        return Err(ChannelError::Permanent(format!(
            "Telegram rejected the message ({status})"
        )));
    }
    Err(ChannelError::Transient(format!(
        "Telegram is having trouble ({status})"
    )))
}

pub async fn health() -> Health {
    let Some(tok) = token().await else {
        return Health::off(ChannelId::Telegram);
    };
    let url = format!("{}/bot{}/getMe", base_url(), tok);
    match reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {
            if owner_user_id().await.is_none() {
                return Health::needs_user(
                    ChannelId::Telegram,
                    "The bot works, but no owner is set, so it will ignore every command it receives.",
                    "Set your Telegram user id so the bot knows which messages are yours.",
                );
            }
            Health::ok(ChannelId::Telegram, "Connected.")
        }
        Ok(r) => Health::needs_user(
            ChannelId::Telegram,
            format!("Telegram refused the bot token ({})", r.status()),
            "Check the token in settings.",
        ),
        Err(e) => Health::down(
            ChannelId::Telegram,
            format!("Cannot reach Telegram: {e}"),
            None,
        ),
    }
}

/// One message someone sent the bot.
#[derive(Debug, Clone)]
pub struct Update {
    pub update_id: i64,
    pub from_user_id: String,
    pub chat_id: String,
    pub text: String,
}

/// Long-poll for messages. `Ok(None)` simply means nothing arrived.
pub async fn get_updates(offset: i64, timeout_s: u64) -> anyhow::Result<Option<Vec<Update>>> {
    let Some(tok) = token().await else {
        return Ok(None);
    };
    let url = format!("{}/bot{}/getUpdates", base_url(), tok);
    let res = reqwest::Client::new()
        .get(&url)
        .query(&[
            ("offset", offset.to_string()),
            ("timeout", timeout_s.to_string()),
            // Only plain messages. Nothing here acts on a button press, so
            // there is no reason to receive one.
            ("allowed_updates", "[\"message\"]".to_string()),
        ])
        .timeout(std::time::Duration::from_secs(timeout_s + 10))
        .send()
        .await?;

    if !res.status().is_success() {
        anyhow::bail!("telegram getUpdates returned {}", res.status());
    }
    let v: serde_json::Value = res.json().await?;
    let out = v["result"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|u| {
                    Some(Update {
                        update_id: u["update_id"].as_i64()?,
                        from_user_id: u["message"]["from"]["id"].as_i64()?.to_string(),
                        chat_id: u["message"]["chat"]["id"].as_i64()?.to_string(),
                        text: u["message"]["text"].as_str().unwrap_or("").to_string(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(Some(out))
}

/// The card a run produces. Short, because it is read on a phone.
pub fn result_card(
    task_name: &str,
    ok: bool,
    summary: &str,
    duration_s: i64,
    cost_usd: f64,
) -> String {
    let head = if ok { "Done" } else { "Could not finish" };
    let mark = if ok { "\u{2705}" } else { "\u{274c}" };
    format!(
        "{mark} <b>{}</b>\n{}\n\n<i>{} \u{b7} {}m {}s \u{b7} ${:.2}</i>",
        html_escape(task_name),
        html_escape(summary.trim()),
        head,
        duration_s / 60,
        duration_s % 60,
        cost_usd
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nobody_is_trusted_until_an_owner_is_set() {
        // A bot is addressable by anyone who finds its username, and the
        // buttons on a card approve real bookings.
        assert!(!is_owner("12345", None));
        assert!(!is_owner("", None));
    }

    #[test]
    fn only_the_configured_owner_is_obeyed() {
        assert!(is_owner("12345", Some("12345")));
        assert!(!is_owner("99999", Some("12345")));
        assert!(!is_owner("12345", Some("")));
    }

    #[test]
    fn the_card_says_what_happened_and_what_it_cost() {
        let c = result_card(
            "Book tennis court",
            true,
            "Court 2 booked for Wed 19:00.",
            102,
            0.14,
        );
        assert!(c.contains("Book tennis court"));
        assert!(c.contains("Court 2 booked"));
        assert!(c.contains("1m 42s"));
        assert!(c.contains("$0.14"));
    }

    #[test]
    fn a_failure_card_is_visibly_a_failure() {
        let c = result_card(
            "Book tennis court",
            false,
            "The site asked for a code.",
            30,
            0.02,
        );
        assert!(c.contains("Could not finish"));
        assert!(c.contains("\u{274c}"));
    }

    #[test]
    fn markup_in_a_task_name_cannot_break_the_card() {
        let c = result_card("<b>hack</b> & co", true, "fine", 1, 0.0);
        assert!(c.contains("&lt;b&gt;hack&lt;/b&gt; &amp; co"));
    }
}

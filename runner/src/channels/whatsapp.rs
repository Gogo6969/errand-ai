//! WhatsApp, through an unofficial gateway.
//!
//! Be clear about what this is. WhatsApp has no personal API, so sending goes
//! through a gateway that drives WhatsApp Web. That means three things the user
//! deserves to know before they switch it on: the session decays and needs a QR
//! rescan that cannot happen unattended, the protocol changes without notice,
//! and automated sending can get a personal number banned. Losing your WhatsApp
//! account is a worse outcome than a message not arriving.
//!
//! So this is best effort. Run outcomes go to Telegram regardless, and WhatsApp
//! is never the only way you find out what happened.

use super::{ChannelError, ChannelId, Health, SendResult};

pub const ACCOUNT_API_KEY: &str = "whatsapp.api_key";
pub const SETTING_BASE_URL: &str = "messaging.whatsapp.base_url";

/// Where the gateway is.
///
/// No default: assuming a particular install is how a public build ends up
/// quietly pointing at somebody else's machine.
pub async fn base_url(pool: &errand_core::db::Pool) -> Option<String> {
    if let Ok(u) = std::env::var("ERRAND_WHATSAPP_API") {
        return Some(u);
    }
    errand_core::db::get_setting(pool, SETTING_BASE_URL)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_str().map(str::to_string))
}

async fn api_key() -> Option<String> {
    crate::secrets::get_internal(ACCOUNT_API_KEY)
        .await
        .ok()
        .map(|s| s.expose().to_string())
}

/// Turn what a person typed into what the gateway wants.
///
/// A group id or an already-qualified id passes through; anything else is
/// treated as a phone number.
pub fn normalise_recipient(input: &str) -> String {
    let t = input.trim();
    if t.contains('@') {
        return t.to_string();
    }
    let digits: String = t.chars().filter(|c| c.is_ascii_digit()).collect();
    format!("{digits}@c.us")
}

pub async fn send(base: Option<String>, recipient: &str, body: &str) -> SendResult {
    let Some(base) = base else {
        return Err(ChannelError::NeedsUser {
            why: "WhatsApp has no gateway configured".into(),
            fix: "Point Errand at your WhatsApp gateway in settings, or leave WhatsApp off and \
                  use Telegram."
                .into(),
        });
    };
    let Some(key) = api_key().await else {
        return Err(ChannelError::NeedsUser {
            why: "WhatsApp has no gateway key saved".into(),
            fix: "Add the gateway's API key in settings.".into(),
        });
    };

    let client = reqwest::Client::new();
    // Find a session that is actually ready. A gateway that is up but logged
    // out will accept the request and silently do nothing.
    let sessions = client
        .get(format!("{base}/sessions"))
        .header("X-API-Key", &key)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| {
            ChannelError::Transient(format!("the WhatsApp gateway is not answering: {e}"))
        })?;

    if !sessions.status().is_success() {
        return Err(ChannelError::Transient(format!(
            "the WhatsApp gateway returned {}",
            sessions.status()
        )));
    }

    let body_text = sessions.text().await.unwrap_or_default();
    let ready = serde_json::from_str::<serde_json::Value>(&body_text)
        .ok()
        .and_then(|v| {
            v.as_array().and_then(|a| {
                a.iter()
                    .find(|s| s["status"] == "ready")
                    .and_then(|s| s["id"].as_str().map(str::to_string))
            })
        });

    let Some(session) = ready else {
        return Err(ChannelError::NeedsUser {
            why: "WhatsApp is not logged in".into(),
            fix: "Open the gateway in a browser and scan the QR code with your phone. This cannot \
                  be done automatically."
                .into(),
        });
    };

    let res = client
        .post(format!("{base}/sessions/{session}/messages/send-text"))
        .header("X-API-Key", &key)
        .json(&serde_json::json!({
            "chatId": normalise_recipient(recipient),
            "text": body,
        }))
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| ChannelError::Transient(format!("the send did not go through: {e}")))?;

    let status = res.status();
    if status.is_success() {
        let id = res
            .text()
            .await
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v["id"].as_str().map(str::to_string))
            .unwrap_or_default();
        Ok(id)
    } else if status.is_client_error() {
        Err(ChannelError::Permanent(format!(
            "the gateway rejected that recipient or message ({status})"
        )))
    } else {
        Err(ChannelError::Transient(format!(
            "the gateway is having trouble ({status})"
        )))
    }
}

pub async fn health(base: Option<String>) -> Health {
    let Some(base) = base else {
        return Health::off(ChannelId::Whatsapp);
    };
    let Some(key) = api_key().await else {
        return Health::needs_user(
            ChannelId::Whatsapp,
            "A gateway is configured but no key is saved.",
            "Add the gateway's API key in settings.",
        );
    };
    match reqwest::Client::new()
        .get(format!("{base}/sessions"))
        .header("X-API-Key", key)
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {
            let text = r.text().await.unwrap_or_default();
            let ready = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| {
                    v.as_array()
                        .map(|a| a.iter().any(|s| s["status"] == "ready"))
                })
                .unwrap_or(false);
            if ready {
                Health::ok(ChannelId::Whatsapp, "Logged in and ready.")
            } else {
                Health::needs_user(
                    ChannelId::Whatsapp,
                    "The gateway is running but WhatsApp is logged out.",
                    "Open the gateway in a browser and scan the QR code with your phone.",
                )
            }
        }
        Ok(r) => Health::down(
            ChannelId::Whatsapp,
            format!("The gateway returned {}", r.status()),
            None,
        ),
        Err(e) => Health::down(
            ChannelId::Whatsapp,
            format!("The gateway is not answering: {e}"),
            Some("Check that it is running.".into()),
        ),
    }
}

/// Shown before anyone switches this on. Understating it would be dishonest.
pub const RISK_NOTICE: &str = "WhatsApp has no official API for personal accounts, so this sends \
     through a gateway that drives WhatsApp Web. Sessions log themselves out and need a QR rescan, \
     which cannot happen while you are asleep, and automated sending can get a personal number \
     banned. Errand always reports run outcomes over Telegram as well, so WhatsApp is never the \
     only way you find out what happened.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_phone_number_becomes_a_chat_id() {
        assert_eq!(normalise_recipient("+1 555 0100"), "15550100@c.us");
        assert_eq!(normalise_recipient("15550100"), "15550100@c.us");
    }

    #[test]
    fn an_already_qualified_id_is_left_alone() {
        assert_eq!(normalise_recipient("15550100@c.us"), "15550100@c.us");
        assert_eq!(normalise_recipient("1234567890@g.us"), "1234567890@g.us");
    }

    #[test]
    fn the_risk_notice_says_the_thing_people_need_to_hear() {
        assert!(RISK_NOTICE.contains("banned"));
        assert!(RISK_NOTICE.contains("QR"));
        assert!(RISK_NOTICE.contains("never the only way"));
    }
}

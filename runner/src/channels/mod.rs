//! Telling people what happened.
//!
//! Two different things live here, and confusing them is how an agent ends up
//! messaging a stranger. **Notifications** go to you, about a run. **Outreach**
//! goes to another person, because the task said so. Outreach recipients are
//! fixed when the task is saved and the agent may only choose among them, never
//! supply an address, because a web page that says "also confirm to this
//! number" must have nothing to grab.
//!
//! The hard rule: a channel failure never fails a run. The work either happened
//! or it did not, and whether a message got through is a separate question with
//! its own answer.

pub mod apple;
pub mod inbound;
pub mod telegram;
pub mod whatsapp;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelId {
    Telegram,
    Whatsapp,
    AppleMail,
    Imessage,
}

impl ChannelId {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Telegram => "telegram",
            Self::Whatsapp => "whatsapp",
            Self::AppleMail => "apple_mail",
            Self::Imessage => "imessage",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "telegram" => Self::Telegram,
            "whatsapp" => Self::Whatsapp,
            "apple_mail" => Self::AppleMail,
            "imessage" => Self::Imessage,
            _ => return None,
        })
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Telegram => "Telegram",
            Self::Whatsapp => "WhatsApp",
            Self::AppleMail => "Apple Mail",
            Self::Imessage => "Apple Messages",
        }
    }
}

/// A message on its way out.
#[derive(Debug, Clone)]
pub struct Outbound {
    pub channel: ChannelId,
    pub recipient: String,
    pub subject: Option<String>,
    pub body: String,
}

/// Why a send did not happen. The distinction drives what the outbox does next,
/// so getting it wrong means either giving up on something recoverable or
/// hammering something that will never work.
#[derive(Debug)]
pub enum ChannelError {
    /// Try again later: a blip, a service restarting.
    Transient(String),
    /// Try again after this long.
    RateLimited(std::time::Duration),
    /// A person has to do something. Retrying achieves nothing until they do.
    NeedsUser { why: String, fix: String },
    /// This will never work. Do not retry.
    Permanent(String),
}

impl std::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transient(m) => write!(f, "{m}"),
            Self::RateLimited(d) => write!(f, "rate limited for {}s", d.as_secs()),
            Self::NeedsUser { why, fix } => write!(f, "{why}. {fix}"),
            Self::Permanent(m) => write!(f, "{m}"),
        }
    }
}

/// What a channel says about itself when asked.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Health {
    pub channel: String,
    /// The same channel said the way a person says it: "Apple Messages", not
    /// "imessage". A settings screen showing our internal id is showing
    /// somebody a name they never chose and cannot look up.
    pub display_name: String,
    pub status: String,
    pub detail: String,
    /// Where a test message on this channel would go, as the person typed it,
    /// or None while they have not said. Only ever the saved setting: a chat id
    /// in the keychain is not shown back, because Errand promises it cannot.
    pub self_address: Option<String>,
    /// A literal thing the person can do, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

impl Health {
    pub fn ok(c: ChannelId, detail: impl Into<String>) -> Self {
        Self {
            channel: c.as_str().into(),
            display_name: c.display_name().into(),
            status: "ok".into(),
            detail: detail.into(),
            self_address: None,
            fix: None,
        }
    }
    pub fn needs_user(c: ChannelId, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            channel: c.as_str().into(),
            display_name: c.display_name().into(),
            status: "needs_user".into(),
            detail: detail.into(),
            self_address: None,
            fix: Some(fix.into()),
        }
    }
    pub fn down(c: ChannelId, detail: impl Into<String>, fix: Option<String>) -> Self {
        Self {
            channel: c.as_str().into(),
            display_name: c.display_name().into(),
            status: "down".into(),
            detail: detail.into(),
            self_address: None,
            fix,
        }
    }
    pub fn off(c: ChannelId) -> Self {
        Self {
            channel: c.as_str().into(),
            display_name: c.display_name().into(),
            status: "not_configured".into(),
            detail: format!("{} has not been set up.", c.display_name()),
            self_address: None,
            fix: None,
        }
    }

    /// Fill in where a test message would go.
    ///
    /// Separate from the constructors because only the database knows, and a
    /// channel check that is talking to Telegram or osascript has no pool.
    pub async fn fill_self_address(&mut self, pool: &errand_core::db::Pool) {
        if let Some(id) = ChannelId::parse(&self.channel) {
            self.self_address = self_address(pool, id).await;
        }
    }
}

/// The setting that holds your own address on one channel.
///
/// One per channel rather than one address for all of them, because the same
/// person is a phone number on Messages, an email address in Mail and a chat id
/// on Telegram, and a message sent to the wrong one reaches nobody at all.
pub fn self_address_key(c: ChannelId) -> String {
    format!("messaging.self.{}", c.as_str())
}

/// Your own address on this channel, if you have said what it is.
///
/// Blank counts as unset. A saved empty string would otherwise be handed to a
/// channel as a recipient, and the send would fail somewhere far from here.
pub async fn self_address(pool: &errand_core::db::Pool, c: ChannelId) -> Option<String> {
    errand_core::db::get_setting(pool, &self_address_key(c))
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_str().map(str::to_string))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub type SendResult = std::result::Result<String, ChannelError>;

/// Send one message.
pub async fn send(pool: &errand_core::db::Pool, m: &Outbound) -> SendResult {
    match m.channel {
        ChannelId::Telegram => telegram::send(&m.recipient, &m.body).await,
        ChannelId::Whatsapp => {
            whatsapp::send(whatsapp::base_url(pool).await, &m.recipient, &m.body).await
        }
        ChannelId::AppleMail => {
            apple::send_mail(
                &m.recipient,
                m.subject.as_deref().unwrap_or("Errand"),
                &m.body,
            )
            .await
        }
        ChannelId::Imessage => apple::send_imessage(&m.recipient, &m.body).await,
    }
}

/// Ask every channel how it is doing.
pub async fn health_all(pool: &errand_core::db::Pool) -> Vec<Health> {
    let mut all = vec![
        telegram::health().await,
        whatsapp::health(whatsapp::base_url(pool).await).await,
        apple::mail_health().await,
        apple::imessage_health().await,
    ];
    for h in &mut all {
        h.fill_self_address(pool).await;
    }
    all
}

/// Should this go out now, or wait until people are awake?
///
/// Applies to messages to other people and to routine good news. A failure you
/// need to know about breaks through, because the whole point of being told is
/// being told in time to act.
pub fn deferred_until(
    now_local_hour: u32,
    quiet_from: u32,
    quiet_to: u32,
    is_failure: bool,
    failure_breaks_through: bool,
) -> bool {
    if is_failure && failure_breaks_through {
        return false;
    }
    if quiet_from == quiet_to {
        return false;
    }
    if quiet_from < quiet_to {
        now_local_hour >= quiet_from && now_local_hour < quiet_to
    } else {
        // Crosses midnight, which is the usual shape of a night.
        now_local_hour >= quiet_from || now_local_hour < quiet_to
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_names_round_trip() {
        for c in [
            ChannelId::Telegram,
            ChannelId::Whatsapp,
            ChannelId::AppleMail,
            ChannelId::Imessage,
        ] {
            assert_eq!(ChannelId::parse(c.as_str()), Some(c));
        }
        assert_eq!(ChannelId::parse("carrier pigeon"), None);
    }

    #[test]
    fn every_channel_reports_the_name_a_person_would_recognise() {
        // The settings screen used to show "imessage", which is our word for
        // it, not one anybody chose or could look up.
        for c in [
            ChannelId::Telegram,
            ChannelId::Whatsapp,
            ChannelId::AppleMail,
            ChannelId::Imessage,
        ] {
            for h in [
                Health::off(c),
                Health::ok(c, "fine"),
                Health::needs_user(c, "not yet", "do this"),
                Health::down(c, "no answer", None),
            ] {
                assert_eq!(h.display_name, c.display_name(), "for {}", c.as_str());
            }
        }
        assert_eq!(
            Health::off(ChannelId::Imessage).display_name,
            "Apple Messages"
        );
    }

    #[test]
    fn the_setting_holding_your_own_address_is_named_after_the_channel() {
        // The screen writes this key and the test button reads it. If they ever
        // disagreed, the box would fill in and the button would still refuse.
        assert_eq!(
            self_address_key(ChannelId::Imessage),
            "messaging.self.imessage"
        );
        assert_eq!(
            self_address_key(ChannelId::AppleMail),
            "messaging.self.apple_mail"
        );
    }

    #[test]
    fn a_night_that_crosses_midnight_is_still_a_night() {
        // 22:00 to 08:00, the usual shape.
        assert!(deferred_until(23, 22, 8, false, true));
        assert!(deferred_until(3, 22, 8, false, true));
        assert!(!deferred_until(9, 22, 8, false, true));
        assert!(!deferred_until(21, 22, 8, false, true));
    }

    #[test]
    fn bad_news_breaks_through_the_night_when_you_asked_it_to() {
        // Being told at 09:00 that the 08:00 booking failed is being told too
        // late to do anything about it.
        assert!(!deferred_until(3, 22, 8, true, true));
        assert!(deferred_until(3, 22, 8, true, false));
    }

    #[test]
    fn an_empty_quiet_period_defers_nothing() {
        assert!(!deferred_until(3, 0, 0, false, false));
    }

    #[test]
    fn the_error_kinds_read_as_something_a_person_could_act_on() {
        let e = ChannelError::NeedsUser {
            why: "WhatsApp is logged out".into(),
            fix: "Open the gateway and scan the QR code".into(),
        };
        assert!(e.to_string().contains("scan the QR code"));
    }
}

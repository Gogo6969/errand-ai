//! Telling other programs what happened.
//!
//! A client that holds a stream open sees everything live. A client that
//! restarts, or that only wants to hear about outcomes, subscribes here
//! instead. KinAI uses both: the stream for a live ticker in chat, and a
//! webhook so a restart mid-run does not lose the ending.
//!
//! Two constraints shape this. Targets are restricted to your own machine or
//! network, because a URL a client supplies and we then fetch is the shape of a
//! request-forgery hole. And every delivery is signed, so the receiver can tell
//! a genuine Errand callback from anything else that finds the port.

use crate::state::AppState;

const BACKOFF_S: &[u64] = &[30, 120, 600, 3600];
const TICK: std::time::Duration = std::time::Duration::from_secs(10);

pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(TICK).await;
            if let Err(e) = tick(&state).await {
                tracing::error!("webhook delivery failed: {e}");
            }
        }
    });
}

async fn tick(state: &AppState) -> anyhow::Result<()> {
    let due = errand_core::db::due_deliveries(state.pool(), 10).await?;
    for d in due {
        // Checked again at delivery time, not only at subscribe time: a hook
        // stored before this rule existed, or a row edited by hand, must not
        // become a way to make Errand fetch an arbitrary URL.
        if !errand_core::db::webhook_target_allowed(&d.url) {
            errand_core::db::fail_delivery(
                state.pool(),
                &d.id,
                &d.webhook_id,
                "that address is not on your own machine or network",
                None,
            )
            .await?;
            continue;
        }

        let ts = chrono::Utc::now().timestamp().to_string();
        let sig = sign(state, &d.webhook_id, &ts, &d.payload).await;

        let res = reqwest::Client::new()
            .post(&d.url)
            .header("Content-Type", "application/json")
            .header("X-Errand-Event", &d.event)
            .header("X-Errand-Delivery", &d.id)
            .header("X-Errand-Timestamp", &ts)
            .header("X-Errand-Signature", format!("sha256={sig}"))
            .body(d.payload.clone())
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await;

        match res {
            Ok(r) if r.status().is_success() => {
                errand_core::db::mark_delivered(state.pool(), &d.id, r.status().as_u16()).await?;
            }
            Ok(r) => {
                let next = BACKOFF_S
                    .get(d.attempts as usize)
                    .map(|s| iso_in(*s as i64));
                let disabled = errand_core::db::fail_delivery(
                    state.pool(),
                    &d.id,
                    &d.webhook_id,
                    &format!("the receiver returned {}", r.status()),
                    next,
                )
                .await?;
                if disabled {
                    notify_disabled(state, &d.url).await;
                }
            }
            Err(e) => {
                let next = BACKOFF_S
                    .get(d.attempts as usize)
                    .map(|s| iso_in(*s as i64));
                let disabled = errand_core::db::fail_delivery(
                    state.pool(),
                    &d.id,
                    &d.webhook_id,
                    &format!("could not reach the receiver: {e}"),
                    next,
                )
                .await?;
                if disabled {
                    notify_disabled(state, &d.url).await;
                }
            }
        }
    }
    Ok(())
}

/// A hook that has been switched off is worth saying out loud, because the
/// symptom otherwise is another program silently not hearing about anything.
async fn notify_disabled(state: &AppState, url: &str) {
    tracing::warn!(url, "webhook disabled after repeated failures");
    if let Some(chat) = crate::channels::telegram::configured_chat_id().await {
        let _ = errand_core::db::enqueue_message(
            state.pool(),
            errand_core::db::NewMessage {
                run_id: None,
                task_id: None,
                class: "notify".into(),
                channel: "telegram".into(),
                recipient: chat,
                recipient_label: Some("you".into()),
                subject: None,
                body: format!(
                    "\u{26a0} A program subscribed to Errand at {url} has not answered for a long \
                     time, so Errand has stopped calling it. Whatever it was doing with your run \
                     results is not happening any more."
                ),
                is_failure: true,
            },
        )
        .await;
    }
}

/// HMAC-SHA256 over timestamp and body, so a receiver can tell a real callback
/// from anything else that finds the port, and can reject a replayed one.
async fn sign(state: &AppState, webhook_id: &str, ts: &str, body: &str) -> String {
    let secret = state.webhook_secret(webhook_id).await.unwrap_or_default();
    hmac_sha256_hex(secret.as_bytes(), format!("{ts}.{body}").as_bytes())
}

pub fn hmac_sha256_hex(key: &[u8], msg: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    const BLOCK: usize = 64;
    let mut k = key.to_vec();
    if k.len() > BLOCK {
        k = Sha256::digest(&k).to_vec();
    }
    k.resize(BLOCK, 0);
    let ipad: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();

    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(msg);
    let inner = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(inner);
    hex::encode(outer.finalize())
}

fn iso_in(secs: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(secs)).to_rfc3339()
}

/// Queue this event to everyone who asked for it.
pub async fn emit(state: &AppState, event: &str, payload: serde_json::Value) {
    match errand_core::db::fan_out_event(state.pool(), event, &payload).await {
        Ok(n) if n > 0 => tracing::debug!(event, subscribers = n, "queued webhook deliveries"),
        Ok(_) => {}
        Err(e) => tracing::warn!("could not queue webhooks for {event}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_signature_matches_a_known_value() {
        // Guards the hand-rolled HMAC against a silent breakage.
        assert_eq!(
            hmac_sha256_hex(b"key", b"The quick brown fox jumps over the lazy dog"),
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }

    #[test]
    fn a_long_key_is_handled_like_the_standard_says() {
        let long = vec![b'a'; 200];
        let sig = hmac_sha256_hex(&long, b"hello");
        assert_eq!(sig.len(), 64);
        assert_ne!(sig, hmac_sha256_hex(b"a", b"hello"));
    }

    #[test]
    fn the_signature_covers_the_timestamp_so_a_replay_is_detectable() {
        let a = hmac_sha256_hex(b"s", b"1000.{}");
        let b = hmac_sha256_hex(b"s", b"2000.{}");
        assert_ne!(a, b);
    }

    #[test]
    fn backoff_is_finite() {
        assert!(BACKOFF_S.windows(2).all(|w| w[1] > w[0]));
        assert!(BACKOFF_S.len() <= 5);
    }
}

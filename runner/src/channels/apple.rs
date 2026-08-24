//! Apple Mail and Apple Messages, through AppleScript.
//!
//! The awkward part is not the scripting, it is the permission. macOS binds
//! Automation consent to the code identity of the process that sends the Apple
//! Event, and this daemon runs under launchd with no window. So the prompt is
//! deliberately triggered from a button in settings, while the person is
//! looking at the screen, rather than at 03:00 when a run needs it and there is
//! nobody to click Allow.
//!
//! Two failure codes matter and are translated rather than passed through:
//! -1743 is consent refused, and -600 is the app not running.

use super::{ChannelError, ChannelId, Health, SendResult};

/// Run one AppleScript, with a deadline.
///
/// A deadline is not optional here: osascript blocks indefinitely on a consent
/// prompt nobody can see, which is exactly the shape of the bug that made the
/// keychain wedge the whole daemon earlier in this project.
async fn osascript(script: &str, timeout_s: u64) -> std::result::Result<String, ChannelError> {
    let script = script.to_string();
    let task = tokio::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(&script)
        .output();

    let out =
        match tokio::time::timeout(std::time::Duration::from_secs(timeout_s), task).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                return Err(ChannelError::Transient(format!(
                    "could not run osascript: {e}"
                )))
            }
            Err(_) => return Err(ChannelError::NeedsUser {
                why: "macOS did not answer, which usually means a permission prompt is waiting \
                      where nobody can see it"
                    .into(),
                fix: "Open Errand's settings and press the button to enable this channel, so the \
                      prompt appears while you are looking at the screen."
                    .into(),
            }),
        };

    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    Err(translate(&err))
}

/// Turn an AppleScript error into something a person can act on.
pub fn translate(stderr: &str) -> ChannelError {
    if stderr.contains("-1743") || stderr.contains("Not authorized") {
        return ChannelError::NeedsUser {
            why: "macOS has not given Errand permission to control this app".into(),
            fix: "System Settings, Privacy and Security, Automation, and turn on the app for \
                  Errand. Then try again."
                .into(),
        };
    }
    if stderr.contains("-600") || stderr.contains("isn't running") {
        return ChannelError::Transient("the app was not running; it will be started".into());
    }
    if stderr.contains("-1728") {
        return ChannelError::Permanent(
            "that account or recipient does not exist as far as the app is concerned".into(),
        );
    }
    ChannelError::Transient(stderr.trim().chars().take(200).collect())
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ------------------------------------------------------------------- mail --

pub async fn send_mail(to: &str, subject: &str, body: &str) -> SendResult {
    if std::env::var("ERRAND_APPLE_DRY").is_ok() {
        // Used by the test suite so verifying the plumbing never sends anyone
        // a real email.
        return Ok(format!("dry:{to}"));
    }
    let script = format!(
        r#"tell application "Mail"
            set m to make new outgoing message with properties {{subject:"{}", content:"{}", visible:false}}
            tell m to make new to recipient at end of to recipients with properties {{address:"{}"}}
            send m
            return "sent"
        end tell"#,
        escape(subject),
        escape(body),
        escape(to)
    );
    osascript(&script, 45).await.map(|_| "sent".to_string())
}

pub async fn mail_health() -> Health {
    if !cfg!(target_os = "macos") {
        return Health::off(ChannelId::AppleMail);
    }
    match osascript(
        r#"tell application "Mail" to return (count of accounts)"#,
        20,
    )
    .await
    {
        Ok(n) if n.trim() != "0" => Health::ok(
            ChannelId::AppleMail,
            format!("Mail is set up with {n} account(s)."),
        ),
        Ok(_) => Health::needs_user(
            ChannelId::AppleMail,
            "Mail is running but has no accounts.",
            "Add an account in Mail, then try again.",
        ),
        Err(ChannelError::NeedsUser { why, fix }) => {
            Health::needs_user(ChannelId::AppleMail, why, fix)
        }
        Err(e) => Health::down(ChannelId::AppleMail, e.to_string(), None),
    }
}

// --------------------------------------------------------------- messages --

pub async fn send_imessage(to: &str, body: &str) -> SendResult {
    if std::env::var("ERRAND_APPLE_DRY").is_ok() {
        return Ok(format!("dry:{to}"));
    }
    let script = format!(
        r#"tell application "Messages"
            set svc to 1st account whose service type = iMessage
            send "{}" to participant "{}" of svc
            return "sent"
        end tell"#,
        escape(body),
        escape(to)
    );
    osascript(&script, 45).await.map(|_| "sent".to_string())
}

pub async fn imessage_health() -> Health {
    if !cfg!(target_os = "macos") {
        return Health::off(ChannelId::Imessage);
    }
    match osascript(
        r#"tell application "Messages" to return (count of (accounts whose service type = iMessage))"#,
        20,
    )
    .await
    {
        Ok(n) if n.trim() != "0" => Health::ok(ChannelId::Imessage, "Messages is signed in."),
        Ok(_) => Health::needs_user(
            ChannelId::Imessage,
            "Messages is running but not signed in to iMessage.",
            "Open Messages and sign in with your Apple ID.",
        ),
        Err(ChannelError::NeedsUser { why, fix }) => {
            Health::needs_user(ChannelId::Imessage, why, fix)
        }
        Err(e) => Health::down(ChannelId::Imessage, e.to_string(), None),
    }
}

/// Ask for Automation consent now, while the person is watching.
///
/// Harmless on purpose: it reads a count and sends nothing. The point is to
/// make macOS show its prompt at a moment when there is somebody to answer it.
pub async fn request_consent(which: ChannelId) -> Health {
    match which {
        ChannelId::AppleMail => mail_health().await,
        ChannelId::Imessage => imessage_health().await,
        other => Health::off(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refused_consent_says_exactly_where_to_go() {
        let e = translate("execution error: Not authorized to send Apple events (-1743)");
        match e {
            ChannelError::NeedsUser { fix, .. } => {
                assert!(fix.contains("Privacy and Security"));
                assert!(fix.contains("Automation"));
            }
            other => panic!("expected NeedsUser, got {other:?}"),
        }
    }

    #[test]
    fn an_app_that_is_not_running_is_only_a_hiccup() {
        assert!(matches!(
            translate("Application isn't running (-600)"),
            ChannelError::Transient(_)
        ));
    }

    #[test]
    fn an_unknown_recipient_is_permanent_rather_than_retried_forever() {
        assert!(matches!(
            translate("Can't get account 1 (-1728)"),
            ChannelError::Permanent(_)
        ));
    }

    #[test]
    fn quotes_in_a_message_cannot_break_out_of_the_script() {
        let s = escape(r#"say "hi" \ bye"#);
        assert_eq!(s, r#"say \"hi\" \\ bye"#);
    }
}

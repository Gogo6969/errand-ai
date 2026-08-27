//! Apple Mail and Apple Messages, through AppleScript, and the permission every
//! app on this Mac needs before Errand can drive it.
//!
//! The awkward part is not the scripting, it is the permission. macOS binds
//! Automation consent to the code identity of the process that sends the Apple
//! Event, and this daemon runs under launchd with no window. So the prompt is
//! deliberately triggered from a button in settings, while the person is
//! looking at the screen, rather than at 03:00 when a run needs it and there is
//! nobody to click Allow.
//!
//! That is why the words for a refusal live here rather than beside each thing
//! that hits one. Notes and the mail triage tools drive apps too, and a person
//! reading "it did not work" at breakfast needs the same sentence naming the
//! same app and the same button whichever tool met the wall.
//!
//! Two failure codes matter and are translated rather than passed through:
//! -1743 is consent refused, and -600 is the app not running.

use serde::{Deserialize, Serialize};

use super::{ChannelError, ChannelId, Health, SendResult};

// ------------------------------------------------------- being allowed in --

/// The two ways macOS says no to driving an app.
///
/// Both mean one thing to the person and one thing to the agent: permission
/// that was never given. The words are built from these two constants so that
/// everything which has to recognise a refusal afterwards, the tools and the
/// end of a run included, can look for something fixed rather than guess at
/// prose somebody may reword next month.
pub const REFUSED: &str = "macOS has not given Errand permission to control";
pub const NO_ANSWER: &str = "macOS did not answer about";

/// What to do about it, in the order to try it.
///
/// Enable first, because that is the only place the prompt can appear while
/// there is somebody to answer it. System Settings second, because once a
/// prompt has been refused macOS never asks again, and Enable then looks like
/// a button that does nothing.
pub fn permission_fix(app: &str) -> String {
    format!(
        "Open Errand's settings and press Enable next to {app}, so the prompt appears while you \
         are looking at the screen. If no prompt appears, macOS has already been told no once: \
         switch Errand on for {app} in System Settings, Privacy and Security, Automation."
    )
}

/// macOS said no.
pub fn refused(app: &str) -> ChannelError {
    ChannelError::NeedsUser {
        why: format!("{REFUSED} {app}"),
        fix: permission_fix(app),
    }
}

/// macOS said nothing at all, which is a prompt waiting where nobody can see it.
pub fn no_answer(app: &str) -> ChannelError {
    ChannelError::NeedsUser {
        why: format!(
            "{NO_ANSWER} {app}, which usually means a permission prompt is waiting where nobody \
             can see it"
        ),
        fix: permission_fix(app),
    }
}

/// Is this macOS withholding permission, rather than anything about the job?
///
/// Two callers depend on the answer: a tool, which has to tell the agent that
/// retrying is pointless, and the end of a run, which must not record a success
/// when the thing the person asked for never happened.
pub fn is_permission_block(message: &str) -> bool {
    message.contains(REFUSED) || message.contains(NO_ANSWER)
}

/// Run one AppleScript, with a deadline.
///
/// A deadline is not optional here: osascript blocks indefinitely on a consent
/// prompt nobody can see, which is exactly the shape of the bug that made the
/// keychain wedge the whole daemon earlier in this project.
async fn osascript(
    app: &str,
    script: &str,
    timeout_s: u64,
) -> std::result::Result<String, ChannelError> {
    let script = script.to_string();
    let task = tokio::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(&script)
        .output();

    let out = match tokio::time::timeout(std::time::Duration::from_secs(timeout_s), task).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Err(ChannelError::Transient(format!(
                "could not run osascript: {e}"
            )))
        }
        Err(_) => return Err(no_answer(app)),
    };

    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    Err(translate(app, &err))
}

/// Turn an AppleScript error into something a person can act on.
///
/// The app is named rather than called "this app": somebody reading a failed
/// run at breakfast has to know which switch to look for, and "an app" sends
/// them hunting through a list of thirty.
pub fn translate(app: &str, stderr: &str) -> ChannelError {
    if stderr.contains("-1743") || stderr.contains("Not authorized") {
        return refused(app);
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

/// What a channel check answers when nothing may touch the real machine.
///
/// The same answer `app_consent` gives, for the same reason: asking macOS is
/// itself what makes it prompt, so a rehearsal must not ask. A check looks
/// harmless next to a send, which is how these two probes kept their osascript
/// call long after the sends beside them stopped making one, and how a single
/// test asking for the channel list could open Mail and Messages on the machine
/// of whoever ran the suite.
fn rehearsal(c: ChannelId) -> Health {
    match crate::desktop::rehearsed_refusal() {
        Some(stderr) => from_channel_error(c, translate(c.display_name(), &stderr)),
        None => Health {
            status: "not_configured".into(),
            detail: format!(
                "This is a rehearsal, so macOS was not asked about {}.",
                c.display_name()
            ),
            ..Health::off(c)
        },
    }
}

/// A refusal or a failure, said the way a channel says it.
///
/// Shared so that a rehearsed refusal and a real one reach the screen in one
/// set of words rather than two that can drift apart.
fn from_channel_error(c: ChannelId, e: ChannelError) -> Health {
    match e {
        ChannelError::NeedsUser { why, fix } => Health::needs_user(c, why, fix),
        other => Health::down(c, other.to_string(), None),
    }
}

// ------------------------------------------------------------------- mail --

pub async fn send_mail(to: &str, subject: &str, body: &str) -> SendResult {
    if crate::desktop::dry() {
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
    osascript(ChannelId::AppleMail.display_name(), &script, 45)
        .await
        .map(|_| "sent".to_string())
}

pub async fn mail_health() -> Health {
    if !cfg!(target_os = "macos") {
        return Health::off(ChannelId::AppleMail);
    }
    if crate::desktop::dry() {
        return rehearsal(ChannelId::AppleMail);
    }
    match osascript(
        ChannelId::AppleMail.display_name(),
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
        Err(e) => from_channel_error(ChannelId::AppleMail, e),
    }
}

// --------------------------------------------------------------- messages --

pub async fn send_imessage(to: &str, body: &str) -> SendResult {
    if crate::desktop::dry() {
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
    osascript(ChannelId::Imessage.display_name(), &script, 45)
        .await
        .map(|_| "sent".to_string())
}

pub async fn imessage_health() -> Health {
    if !cfg!(target_os = "macos") {
        return Health::off(ChannelId::Imessage);
    }
    if crate::desktop::dry() {
        return rehearsal(ChannelId::Imessage);
    }
    match osascript(
        ChannelId::Imessage.display_name(),
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
        Err(e) => from_channel_error(ChannelId::Imessage, e),
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

// ----------------------------------------- apps a task drives, not channels --

/// An app on this Mac a task drives, and the permission it needs first.
///
/// Not a channel: nothing here messages anybody. What it shares with a channel
/// is the awkward part, which is that macOS grants Automation to whichever
/// process sends the Apple Event. So consent is asked for here, from the
/// daemon, and never from the window.
///
/// Reading the mail is listed apart from sending it deliberately. macOS grants
/// Automation for one app at a time rather than for one thing you do with it,
/// so in practice the same grant covers both, but Errand checks each on its own
/// instead of announcing that one implies the other and being wrong about
/// somebody's Mac.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Automation {
    Notes,
    MailReading,
}

impl Automation {
    pub const ALL: [Self; 2] = [Self::Notes, Self::MailReading];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Notes => "notes",
            Self::MailReading => "mail_reading",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "notes" => Self::Notes,
            "mail_reading" => Self::MailReading,
            _ => return None,
        })
    }

    /// The app itself, as macOS names it. This is the word that goes in a
    /// refusal, because it is the word on the switch the person has to find.
    pub fn app_name(&self) -> &'static str {
        match self {
            Self::Notes => "Apple Notes",
            Self::MailReading => "Apple Mail",
        }
    }

    /// What the card on the settings screen is called, and the doctor line.
    /// Longer than the app's name for the mail, because a second card simply
    /// saying "Apple Mail" beside the one for sending would look like a
    /// duplicate of it.
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Notes => "Apple Notes",
            Self::MailReading => "Apple Mail (reading your post)",
        }
    }
}

/// What one app says when asked whether Errand may drive it.
///
/// The field names match a channel's `Health` so the screen can draw both the
/// same way. There is no self_address, because nothing here is a way of
/// reaching anybody.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutomationHealth {
    pub app: String,
    pub display_name: String,
    pub status: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

impl AutomationHealth {
    fn new(app: Automation, status: &str, detail: impl Into<String>, fix: Option<String>) -> Self {
        Self {
            app: app.as_str().into(),
            display_name: app.display_name().into(),
            status: status.into(),
            detail: detail.into(),
            fix,
        }
    }

    pub fn is_ok(&self) -> bool {
        self.status == "ok"
    }
}

/// Ask macOS whether Errand may drive one app, letting it prompt if it never
/// has been asked.
///
/// The check and the request are one call on purpose: there is no way to put
/// the question to macOS quietly, and no reason to want one. It reads a count
/// and changes nothing, which is what makes it safe to run on every settings
/// screen and every doctor; and macOS remembers its answer, so asking twice
/// never puts a second prompt on somebody's screen.
///
/// Called from the daemon and nowhere else, for the reason at the top of this
/// file: asking from the window would grant the window permission, and the
/// 03:00 run would still fail.
pub async fn app_consent(app: Automation) -> AutomationHealth {
    if !cfg!(target_os = "macos") {
        return AutomationHealth::new(
            app,
            "not_configured",
            "This is not a Mac, so there is nothing here to allow.",
            None,
        );
    }
    if crate::desktop::dry() {
        // A rehearsal, including the one the tests run in. Asking macOS would
        // put a prompt on the screen of whoever is running the suite.
        return match crate::desktop::rehearsed_refusal() {
            Some(stderr) => from_error(app, translate(app.app_name(), &stderr)),
            None => AutomationHealth::new(
                app,
                "not_configured",
                format!(
                    "This is a rehearsal, so macOS was not asked about {}.",
                    app.app_name()
                ),
                None,
            ),
        };
    }

    match app {
        Automation::Notes => match osascript(
            app.app_name(),
            r#"tell application "Notes" to return (count of accounts)"#,
            20,
        )
        .await
        {
            Ok(n) if n.trim() != "0" => AutomationHealth::new(
                app,
                "ok",
                "Errand may write notes. A task that is asked to write something down can put it \
                 in Notes.",
                None,
            ),
            Ok(_) => AutomationHealth::new(
                app,
                "needs_user",
                "Notes answered, but it has no account, so there is nowhere to put a note.",
                Some("Open Notes on the Mac once and sign in, then press Enable again.".into()),
            ),
            Err(e) => from_error(app, e),
        },

        // Deliberately not the same script as the mail channel's. That one
        // counts accounts, which is what sending needs; this reaches into an
        // account for its mailboxes, which is what reading needs, so the answer
        // is about the thing the tools actually do.
        Automation::MailReading => match osascript(
            app.app_name(),
            r#"tell application "Mail"
                if (count of accounts) is 0 then return "!no-accounts"
                return (count of mailboxes of account 1) as text
            end tell"#,
            30,
        )
        .await
        {
            Ok(said) if said.trim() == "!no-accounts" => AutomationHealth::new(
                app,
                "needs_user",
                "Mail answered, but it has no account, so there is no post to read.",
                Some("Add an account in Mail, then press Enable again.".into()),
            ),
            Ok(said) => AutomationHealth::new(
                app,
                "ok",
                format!(
                    "Errand may read your mail. Mail listed {} mailbox(es) in its first account.",
                    said.trim()
                ),
                None,
            ),
            Err(e) => from_error(app, e),
        },
    }
}

/// Every app, asked one after another rather than at once: two prompts racing
/// each other onto the screen is how somebody clicks Allow on the one they did
/// not read.
pub async fn all_app_consent() -> Vec<AutomationHealth> {
    let mut out = vec![];
    for app in Automation::ALL {
        out.push(app_consent(app).await);
    }
    out
}

fn from_error(app: Automation, e: ChannelError) -> AutomationHealth {
    match e {
        ChannelError::NeedsUser { why, fix } => {
            AutomationHealth::new(app, "needs_user", why, Some(fix))
        }
        other => AutomationHealth::new(
            app,
            "down",
            format!("{} could not be asked: {other}", app.app_name()),
            Some(format!(
                "Open {} on the Mac once, then press Enable again.",
                app.app_name()
            )),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refused_consent_names_the_app_and_says_exactly_where_to_go() {
        let e = translate(
            "Apple Notes",
            "execution error: Not authorized to send Apple events (-1743)",
        );
        match e {
            ChannelError::NeedsUser { why, fix } => {
                // "this app" used to be as far as it went, which leaves somebody
                // scrolling a list of thirty switches looking for the right one.
                assert!(why.contains("Apple Notes"), "{why}");
                assert!(fix.contains("Enable"), "{fix}");
                assert!(fix.contains("Apple Notes"), "{fix}");
                assert!(fix.contains("Privacy and Security"), "{fix}");
                assert!(fix.contains("Automation"), "{fix}");
            }
            other => panic!("expected NeedsUser, got {other:?}"),
        }
    }

    #[test]
    fn a_mac_that_never_answers_is_a_permission_problem_too() {
        // This is the failure the Bitcoin run actually hit: not a refusal, a
        // prompt waiting on a screen nobody was looking at. Everything that
        // reacts to a refusal has to react to this the same way.
        let refused_words = refused("Apple Notes").to_string();
        let silent_words = no_answer("Apple Notes").to_string();
        assert!(is_permission_block(&refused_words), "{refused_words}");
        assert!(is_permission_block(&silent_words), "{silent_words}");
        assert!(silent_words.contains("Apple Notes"), "{silent_words}");
        assert!(silent_words.contains("Enable"), "{silent_words}");
    }

    #[test]
    fn an_ordinary_hiccup_is_not_mistaken_for_a_permission_problem() {
        // A task that is told to stop trying is a task that stops. Only the two
        // things a person can fix with a switch may say so.
        assert!(!is_permission_block("Mail has no account set up"));
        assert!(!is_permission_block(
            "the app was not running; it will be started"
        ));
    }

    #[test]
    fn an_app_that_is_not_running_is_only_a_hiccup() {
        assert!(matches!(
            translate("Apple Mail", "Application isn't running (-600)"),
            ChannelError::Transient(_)
        ));
    }

    #[test]
    fn an_unknown_recipient_is_permanent_rather_than_retried_forever() {
        assert!(matches!(
            translate("Apple Mail", "Can't get account 1 (-1728)"),
            ChannelError::Permanent(_)
        ));
    }

    #[test]
    fn every_app_errand_drives_can_be_named_in_a_url_and_read_back() {
        for a in Automation::ALL {
            assert_eq!(Automation::parse(a.as_str()), Some(a));
            assert!(!a.display_name().is_empty());
        }
        assert_eq!(Automation::parse("garage door"), None);
    }

    #[tokio::test]
    async fn a_mac_that_will_not_allow_it_says_which_app_and_which_button() {
        // What doctor prints under a failing line, and what the Enable button
        // puts on the card. Both read the same two fields, so both are proved
        // here: a status on its own tells a worried person nothing.
        std::env::set_var("ERRAND_APPLE_DRY", "1");
        for app in Automation::ALL {
            let h = crate::desktop::PRETEND_MACOS_SAID
                .scope(
                    "execution error: Not authorized to send Apple events. (-1743)".to_string(),
                    app_consent(app),
                )
                .await;
            assert_eq!(h.status, "needs_user", "{h:?}");
            assert!(h.detail.contains(app.app_name()), "{h:?}");
            let fix = h.fix.clone().unwrap_or_default();
            assert!(fix.contains("Enable"), "{fix}");
            assert!(fix.contains(app.app_name()), "{fix}");
            assert!(fix.contains("System Settings"), "{fix}");
        }
    }

    #[tokio::test]
    async fn a_rehearsal_never_asks_the_real_mac_whether_mail_and_messages_are_well() {
        // The defect: these two ran osascript whatever the switch said, so a
        // test that only wanted the channel list drove Mail and Messages on the
        // machine of whoever ran it, and could stop on a permission prompt that
        // nobody expected to be asked for.
        std::env::set_var("ERRAND_APPLE_DRY", "1");
        for h in [mail_health().await, imessage_health().await] {
            assert_eq!(h.status, "not_configured", "{h:?}");
            assert!(h.detail.contains("rehearsal"), "{}", h.detail);
        }
    }

    #[tokio::test]
    async fn a_rehearsed_refusal_on_a_channel_names_the_app_and_the_button() {
        // A rehearsal that answered "all well" would hide the one failure these
        // channels actually have, so a Mac that says no has to be rehearsable
        // too, in the words the real one would produce.
        std::env::set_var("ERRAND_APPLE_DRY", "1");
        for (channel, app) in [
            (ChannelId::AppleMail, "Apple Mail"),
            (ChannelId::Imessage, "Apple Messages"),
        ] {
            let check = async move {
                match channel {
                    ChannelId::AppleMail => mail_health().await,
                    _ => imessage_health().await,
                }
            };
            let h = crate::desktop::PRETEND_MACOS_SAID
                .scope(
                    "execution error: Not authorized to send Apple events. (-1743)".to_string(),
                    check,
                )
                .await;
            assert_eq!(h.status, "needs_user", "{h:?}");
            assert!(h.detail.contains(app), "{h:?}");
            let fix = h.fix.clone().unwrap_or_default();
            assert!(fix.contains("Enable"), "{fix}");
            assert!(fix.contains(app), "{fix}");
        }
    }

    #[tokio::test]
    async fn a_rehearsal_never_asks_the_real_mac_about_notes() {
        std::env::set_var("ERRAND_APPLE_DRY", "1");
        let h = app_consent(Automation::Notes).await;
        assert!(!h.is_ok(), "a rehearsal must not report a real permission");
        assert!(h.detail.contains("rehearsal"), "{}", h.detail);
    }

    #[test]
    fn quotes_in_a_message_cannot_break_out_of_the_script() {
        let s = escape(r#"say "hi" \ bye"#);
        assert_eq!(s, r#"say \"hi\" \\ bye"#);
    }
}

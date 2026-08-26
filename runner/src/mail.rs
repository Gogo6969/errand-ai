//! Reading and tidying Apple Mail, through AppleScript.
//!
//! This is the most private thing Errand touches, so the shape of the module is
//! the argument. Listing hands back a short preview and never a whole message,
//! because a preview is enough to decide what a message is and a body is
//! somebody's private correspondence. Reading a body is one message per call,
//! so working through an inbox is a series of counted, journalled decisions
//! rather than one invisible bulk slurp. And filing is the only thing here that
//! changes anything, which is why it is the only thing the fence guards.
//!
//! What is deliberately not here is the policy. Whether the task was granted
//! the mail at all, whether the run is a rehearsal, and what reaches the
//! journal are all decided in `mcp`, alongside every other rule the agent is
//! held to.
//!
//! The AppleScript rules are the ones `channels::apple` and `desktop` already
//! follow, for the same reasons. osascript gets a hard deadline, because it
//! blocks for ever on a consent prompt that appears where nobody is looking.
//! Everything interpolated is escaped, because a mailbox name is typed by a
//! person and a subject line is written by a stranger. And `ERRAND_APPLE_DRY`
//! turns the module into a rehearsal, so the test suite can prove the rules
//! without reading anybody's real post.

use std::fmt;

/// Long enough for Mail to answer about a full inbox from cold, short enough
/// that a wedged run is still a run that ends. Longer than the note timeout
/// next door because a `whose` search asks Mail to walk every mailbox.
const TIMEOUT_S: u64 = 45;

/// The most messages one listing may hand back.
///
/// A ceiling rather than a preference: every row crosses to whichever model is
/// doing the job, so "list the lot" has to be something the tool cannot do.
pub const MOST_AT_ONCE: usize = 50;

/// How much of a message a listing shows.
///
/// Enough to tell a receipt from a newsletter from a real letter, and not
/// enough to be a copy of the post. Cut inside the AppleScript, so the rest of
/// the body never leaves Mail at all.
const PREVIEW_CHARS: usize = 200;

/// The most one message body may run to. A run triages mail; it does not
/// archive it.
const MAX_BODY_CHARS: usize = 20_000;

/// Guards on what is interpolated into a script. Nothing legitimate is anywhere
/// near these, and an enormous argument fails deep inside osascript with an
/// error nobody can read.
const MAX_ID_CHARS: usize = 400;
const MAX_MAILBOX_CHARS: usize = 200;

/// The body of the message the rehearsal inbox hands back.
///
/// Public so a test can prove that no journal line anywhere ever carries the
/// contents of somebody's post.
pub const REHEARSAL_BODY: &str =
    "Errand invented this message so that its own tests never read anybody's real post.";

/// What went wrong, in the words the agent is shown.
#[derive(Debug, Clone)]
pub enum MailError {
    /// The id is not a message Mail can find. Almost always a message that has
    /// been moved or deleted since the listing it came from.
    NoSuchMessage(String),
    /// No mailbox of that name, in any account.
    NoSuchMailbox(String),
    /// macOS or Mail itself said no.
    Machine(String),
}

impl fmt::Display for MailError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchMessage(id) => write!(
                f,
                "Mail has no message with the id {id}. It was probably moved or deleted since you \
                 listed it. List the mailbox again and work from what is there now"
            ),
            Self::NoSuchMailbox(name) => write!(
                f,
                "There is no mailbox called {name:?} in any of the accounts in Mail. Use \
                 list_mail with no mailbox to read the inbox, and use a name exactly as Mail \
                 spells it, such as \"Junk\" or \"Archive\""
            ),
            Self::Machine(why) => write!(f, "{why}"),
        }
    }
}

/// One message, as much of it as a listing is allowed to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub id: String,
    pub sender: String,
    pub subject: String,
    pub date: String,
    pub preview: String,
}

/// Who a message is from and what it is about: the two things a run's timeline
/// is allowed to keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Headers {
    pub sender: String,
    pub subject: String,
}

/// One message body, fetched on purpose, one at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub sender: String,
    pub subject: String,
    pub date: String,
    pub body: String,
}

/// What one listing found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub messages: Vec<Summary>,
    /// Messages Mail handed over with no message id.
    ///
    /// Counted rather than dropped in silence: an id is the only way a later
    /// call can name a message, so these cannot be read or filed, and a person
    /// reading a run that quietly skipped four messages would have no idea.
    pub unaddressable: usize,
}

/// The app whose name goes in a refusal about the post. The switch a person has
/// to find is labelled with this, so nothing here may call it "the app".
const MAIL: &str = "Apple Mail";

/// True when nothing may touch the real machine.
///
/// The same switch the note and file writing use, read through them rather than
/// copied, so one environment variable turns the whole Mac-touching surface
/// into a rehearsal.
fn dry() -> bool {
    crate::desktop::dry()
}

/// A rehearsal of a Mac that will not let Errand near the post.
///
/// The same seam the note writing uses, so one test switch rehearses a refusal
/// everywhere and there is no second set of words to keep right. Returns the
/// refusal to hand back, or None when the rehearsal is an ordinary one.
fn rehearsed_refusal() -> Option<MailError> {
    crate::desktop::rehearsed_refusal().map(|said| from_stderr(&said))
}

/// Make a string safe to sit inside an AppleScript literal.
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Run one AppleScript, with a deadline.
async fn osascript(script: &str) -> Result<String, MailError> {
    let call = tokio::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .output();

    let out = match tokio::time::timeout(std::time::Duration::from_secs(TIMEOUT_S), call).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(MailError::Machine(format!("macOS could not be asked: {e}"))),
        // Not a hang to wait out: this is the prompt nobody can see, and it is
        // the same problem as an outright refusal, so it is said in the same
        // words and recognised by the same check.
        Err(_) => {
            return Err(MailError::Machine(
                crate::channels::apple::no_answer(MAIL).to_string(),
            ))
        }
    };

    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).to_string());
    }
    Err(from_stderr(&String::from_utf8_lossy(&out.stderr)))
}

/// Turn what osascript printed into something a person can act on.
///
/// Shared by the real path and by a rehearsal so both produce one set of words.
fn from_stderr(stderr: &str) -> MailError {
    // -1728 reaches the shared translation as a missing recipient, which is
    // what it means for a message being sent and not what it means here.
    if stderr.contains("-1728") {
        return MailError::Machine(
            "Mail has no account set up, so there is no post to look at. Open Mail on the Mac \
             once and add an account, then this will work."
                .into(),
        );
    }
    // Refused consent and an app that is not running mean the same thing
    // wherever they happen, and are already put into words once for the mail
    // and messages channels. A second copy of those codes here would be a
    // second thing to keep right.
    MailError::Machine(crate::channels::apple::translate(MAIL, stderr).to_string())
}

/// The handlers every script here shares.
///
/// `findBox` answers `missing value` rather than raising, so a mailbox that is
/// not there comes back as a sentence about that mailbox instead of an
/// AppleScript error number. `flat` exists because the reply is parsed a line
/// at a time: a subject with a newline in it would otherwise become two
/// messages, one of them nonsense.
const HANDLERS: &str = r#"on findBox(boxName)
	tell application "Mail"
		try
			return mailbox boxName
		end try
		repeat with acct in accounts
			try
				return mailbox boxName of acct
			end try
		end repeat
	end tell
	return missing value
end findBox

on findMsg(wanted)
	tell application "Mail"
		try
			return first message of inbox whose message id is wanted
		end try
		repeat with acct in accounts
			repeat with mb in mailboxes of acct
				try
					return first message of mb whose message id is wanted
				end try
			end repeat
		end repeat
		repeat with mb in mailboxes
			try
				return first message of mb whose message id is wanted
			end try
		end repeat
	end tell
	return missing value
end findMsg

on flat(v)
	try
		set s to v as text
	on error
		return ""
	end try
	set AppleScript's text item delimiters to {return, linefeed, tab}
	set parts to text items of s
	set AppleScript's text item delimiters to " "
	set s to parts as text
	set AppleScript's text item delimiters to ""
	return s
end flat

on clip(v, howMany)
	try
		set s to v as text
	on error
		return ""
	end try
	if (count of characters of s) > howMany then return (text 1 thru howMany of s)
	return s
end clip
"#;

/// The newest messages in one mailbox, each with a preview and nothing more.
///
/// The order is whatever Mail hands back for that mailbox, which is its own
/// sort order and normally newest first. Errand does not re-sort it, and does
/// not claim to.
pub async fn list(
    mailbox: Option<&str>,
    limit: usize,
    unread_only: bool,
) -> Result<Listing, MailError> {
    let limit = limit.clamp(1, MOST_AT_ONCE);
    if let Some(name) = mailbox {
        check_name(name)?;
    }
    if let Some(refusal) = rehearsed_refusal() {
        return Err(refusal);
    }
    if dry() {
        return Ok(rehearsal_listing(mailbox, limit, unread_only));
    }

    let box_line = match mailbox {
        Some(name) => format!("set theBox to my findBox(\"{}\")", escape(name)),
        None => "tell application \"Mail\" to set theBox to inbox".to_string(),
    };
    let picker = if unread_only {
        "set msgs to (messages of theBox whose read status is false)"
    } else {
        "set msgs to (messages of theBox)"
    };

    let script = format!(
        r#"{handlers}
{box_line}
if theBox is missing value then return "!no-mailbox"
tell application "Mail"
	{picker}
	set howMany to (count of msgs)
	if howMany > {limit} then set howMany to {limit}
	set out to ""
	repeat with i from 1 to howMany
		set m to item i of msgs
		set theId to ""
		try
			set theId to my flat(message id of m)
		end try
		if theId is "" then
			set out to out & "!no-id" & linefeed
		else
			set snd to ""
			set subj to ""
			set dt to ""
			set pv to ""
			try
				set snd to my flat(sender of m)
			end try
			try
				set subj to my flat(subject of m)
			end try
			try
				set dt to my flat((date received of m) as string)
			end try
			try
				set pv to my flat(my clip(content of m, {PREVIEW_CHARS}))
			end try
			set out to out & theId & tab & snd & tab & subj & tab & dt & tab & pv & linefeed
		end if
	end repeat
	return out
end tell
"#,
        handlers = HANDLERS,
    );

    let reply = osascript(&script).await?;
    if reply.trim() == "!no-mailbox" {
        return Err(MailError::NoSuchMailbox(
            mailbox.unwrap_or("inbox").to_string(),
        ));
    }

    let mut messages = vec![];
    let mut unaddressable = 0;
    for line in reply.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if line.trim() == "!no-id" {
            unaddressable += 1;
            continue;
        }
        let mut f = line.split('\t');
        let (Some(id), Some(sender), Some(subject), Some(date)) =
            (f.next(), f.next(), f.next(), f.next())
        else {
            unaddressable += 1;
            continue;
        };
        messages.push(Summary {
            id: id.trim().to_string(),
            sender: sender.trim().to_string(),
            subject: subject.trim().to_string(),
            date: date.trim().to_string(),
            preview: f.next().unwrap_or_default().trim().to_string(),
        });
    }
    Ok(Listing {
        messages,
        unaddressable,
    })
}

/// One message, in full, because somebody decided this one was worth opening.
pub async fn read(id: &str) -> Result<Message, MailError> {
    check_id(id)?;
    if let Some(refusal) = rehearsed_refusal() {
        return Err(refusal);
    }
    if dry() {
        return rehearsal_message(id);
    }

    let script = format!(
        r#"{handlers}
set m to my findMsg("{id}")
if m is missing value then return "!no-message"
tell application "Mail"
	set snd to ""
	set subj to ""
	set dt to ""
	set bod to ""
	try
		set snd to my flat(sender of m)
	end try
	try
		set subj to my flat(subject of m)
	end try
	try
		set dt to my flat((date received of m) as string)
	end try
	try
		set bod to my clip(content of m, {MAX_BODY_CHARS})
	end try
	return snd & tab & subj & tab & dt & linefeed & bod
end tell
"#,
        handlers = HANDLERS,
        id = escape(id),
    );

    let reply = osascript(&script).await?;
    let (head, body) = reply.split_once('\n').unwrap_or((reply.as_str(), ""));
    if head.trim() == "!no-message" {
        return Err(MailError::NoSuchMessage(id.to_string()));
    }
    let mut f = head.split('\t');
    Ok(Message {
        sender: f.next().unwrap_or_default().trim().to_string(),
        subject: f.next().unwrap_or_default().trim().to_string(),
        date: f.next().unwrap_or_default().trim().to_string(),
        body: body.trim_end().to_string(),
    })
}

/// Who one message is from and what it is about, without opening it.
///
/// The journal needs this and only this. An id is no use to somebody reading
/// their run afterwards, and fetching the whole message to write one line of
/// timeline would mean opening post nobody asked to open.
pub async fn describe(id: &str) -> Result<Headers, MailError> {
    check_id(id)?;
    if let Some(refusal) = rehearsed_refusal() {
        return Err(refusal);
    }
    if dry() {
        let m = rehearsal_message(id)?;
        return Ok(Headers {
            sender: m.sender,
            subject: m.subject,
        });
    }

    let script = format!(
        r#"{handlers}
set m to my findMsg("{id}")
if m is missing value then return "!no-message"
tell application "Mail"
	set snd to ""
	set subj to ""
	try
		set snd to my flat(sender of m)
	end try
	try
		set subj to my flat(subject of m)
	end try
	return snd & tab & subj
end tell
"#,
        handlers = HANDLERS,
        id = escape(id),
    );

    let reply = osascript(&script).await?;
    if reply.trim() == "!no-message" {
        return Err(MailError::NoSuchMessage(id.to_string()));
    }
    Ok(headers_from(reply.trim_end()))
}

/// Move one message to another mailbox. The one thing here that changes
/// anything, which is why it is the one thing the fence guards.
///
/// It hands back who the message was from and what it was about, read a moment
/// before the move in the same script. That is what the run's timeline says
/// afterwards, and asking again once the message has gone would mean hunting
/// for it in its new mailbox.
pub async fn file(id: &str, mailbox: &str) -> Result<Headers, MailError> {
    check_id(id)?;
    check_name(mailbox)?;
    if let Some(refusal) = rehearsed_refusal() {
        return Err(refusal);
    }
    if dry() {
        // Still looked up, so a rehearsal refuses an id a real run would refuse
        // rather than waving it through.
        let m = rehearsal_message(id)?;
        return Ok(Headers {
            sender: m.sender,
            subject: m.subject,
        });
    }

    let script = format!(
        r#"{handlers}
set m to my findMsg("{id}")
if m is missing value then return "!no-message"
set theBox to my findBox("{mailbox}")
if theBox is missing value then return "!no-mailbox"
tell application "Mail"
	set snd to ""
	set subj to ""
	try
		set snd to my flat(sender of m)
	end try
	try
		set subj to my flat(subject of m)
	end try
	set mailbox of m to theBox
	return "moved" & tab & snd & tab & subj
end tell
"#,
        handlers = HANDLERS,
        id = escape(id),
        mailbox = escape(mailbox),
    );

    let reply = osascript(&script).await?;
    let reply = reply.trim_end();
    match reply.split('\t').next().unwrap_or_default().trim() {
        "moved" => Ok(headers_from(reply.split_once('\t').map_or("", |(_, r)| r))),
        "!no-message" => Err(MailError::NoSuchMessage(id.to_string())),
        "!no-mailbox" => Err(MailError::NoSuchMailbox(mailbox.to_string())),
        other => Err(MailError::Machine(format!(
            "Mail was asked to move that message and answered {other:?}, so it is not certain \
             where the message is now. Look in Mail before doing anything else with it."
        ))),
    }
}

fn headers_from(line: &str) -> Headers {
    let mut f = line.split('\t');
    Headers {
        sender: f.next().unwrap_or_default().trim().to_string(),
        subject: f.next().unwrap_or_default().trim().to_string(),
    }
}

fn check_id(id: &str) -> Result<(), MailError> {
    if id.trim().is_empty() {
        return Err(MailError::NoSuchMessage("(nothing)".into()));
    }
    if id.chars().count() > MAX_ID_CHARS {
        return Err(MailError::NoSuchMessage(
            "(far too long to be a real one)".into(),
        ));
    }
    Ok(())
}

fn check_name(name: &str) -> Result<(), MailError> {
    if name.trim().is_empty() {
        return Err(MailError::NoSuchMailbox(String::new()));
    }
    if name.chars().count() > MAX_MAILBOX_CHARS {
        return Err(MailError::NoSuchMailbox(
            "(far too long to be a real one)".into(),
        ));
    }
    Ok(())
}

// ------------------------------------------------------------- a rehearsal --
//
// Two invented messages, and every field of them obviously invented. A
// rehearsal inbox that looked like real post would teach the tests, and anybody
// reading them, the wrong thing entirely.

fn rehearsal_listing(mailbox: Option<&str>, limit: usize, unread_only: bool) -> Listing {
    // A mailbox nobody has is still a mailbox nobody has, even in a rehearsal:
    // the two invented messages live in the inbox and in "Junk".
    let known = matches!(mailbox, None | Some("Junk") | Some("INBOX") | Some("Inbox"));
    if !known {
        return Listing {
            messages: vec![],
            unaddressable: 0,
        };
    }
    let mut messages = rehearsal_messages();
    if unread_only {
        messages.truncate(1);
    }
    messages.truncate(limit);
    Listing {
        messages,
        unaddressable: 0,
    }
}

fn rehearsal_messages() -> Vec<Summary> {
    vec![
        Summary {
            id: "errand-rehearsal-1@example.invalid".into(),
            sender: "A Made-Up Sender <nobody@example.invalid>".into(),
            subject: "An invented message, for testing".into(),
            date: "Tuesday 26 August 2026 at 09:00".into(),
            preview: "Errand invented this. There is no such message in anybody's mail.".into(),
        },
        Summary {
            id: "errand-rehearsal-2@example.invalid".into(),
            sender: "Another Made-Up Sender <nobody-else@example.invalid>".into(),
            subject: "A second invented message".into(),
            date: "Tuesday 26 August 2026 at 08:15".into(),
            preview: "Also invented. Nothing here came out of a real mailbox.".into(),
        },
    ]
}

fn rehearsal_message(id: &str) -> Result<Message, MailError> {
    let found = rehearsal_messages()
        .into_iter()
        .find(|m| m.id == id)
        .ok_or_else(|| MailError::NoSuchMessage(id.to_string()))?;
    Ok(Message {
        sender: found.sender,
        subject: found.subject,
        date: found.date,
        body: REHEARSAL_BODY.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mailbox_name_cannot_break_out_of_the_script() {
        // A mailbox is named by a person and a subject line is written by a
        // stranger, so neither may end the literal it sits in.
        assert_eq!(
            escape(r#"Junk" of account "Work"#),
            r#"Junk\" of account \"Work"#
        );
        assert_eq!(escape(r#"back\slash"#), r#"back\\slash"#);
    }

    #[test]
    fn asking_for_the_whole_mailbox_gets_a_bounded_answer() {
        // The clamp is the rule that stops "list everything" being a thing the
        // tool can do, because every row crosses to a model.
        assert_eq!(1000usize.clamp(1, MOST_AT_ONCE), MOST_AT_ONCE);
        assert_eq!(0usize.clamp(1, MOST_AT_ONCE), 1);
    }

    #[test]
    fn a_message_id_that_is_not_a_message_id_is_refused_before_macos_is_asked() {
        assert!(matches!(check_id("  "), Err(MailError::NoSuchMessage(_))));
        assert!(matches!(
            check_id(&"x".repeat(MAX_ID_CHARS + 1)),
            Err(MailError::NoSuchMessage(_))
        ));
        assert!(check_id("<abc@example.com>").is_ok());
    }

    #[test]
    fn a_message_that_has_moved_since_it_was_listed_says_what_to_do_about_it() {
        let said = MailError::NoSuchMessage("<a@b>".into()).to_string();
        assert!(said.contains("<a@b>"), "{said}");
        assert!(
            said.contains("List the mailbox again"),
            "a refusal with nothing to try next just gets retried: {said}"
        );
    }

    #[test]
    fn a_mailbox_that_does_not_exist_is_named_along_with_how_to_spell_one() {
        let said = MailError::NoSuchMailbox("spam".into()).to_string();
        assert!(said.contains("spam"), "{said}");
        assert!(said.contains("Junk"), "{said}");
    }

    #[tokio::test]
    async fn a_rehearsal_reads_invented_post_and_never_the_real_thing() {
        // Belt and braces: this test would touch the tester's own mail if the
        // switch were ever read wrongly, so it asserts the switch first.
        std::env::set_var("ERRAND_APPLE_DRY", "1");
        assert!(dry());

        let listed = list(None, 10, false).await.expect("the invented inbox");
        assert_eq!(listed.messages.len(), 2);
        assert!(
            listed.messages.iter().all(|m| m.sender.contains("invalid")),
            "the rehearsal inbox has to be obviously invented: {:?}",
            listed.messages
        );

        let one = read(&listed.messages[0].id)
            .await
            .expect("an invented body");
        assert_eq!(one.body, REHEARSAL_BODY);

        assert!(
            matches!(
                read("<really@somebody.example>").await,
                Err(MailError::NoSuchMessage(_))
            ),
            "a rehearsal must refuse an id a real run would refuse"
        );
    }
}

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

/// Long enough for Mail to answer about one message from cold, short enough
/// that a wedged run is still a run that ends. Longer than the note timeout
/// next door because Mail is slower to answer than Notes about anything.
const TIMEOUT_S: u64 = 45;

/// What a listing gets instead, which is more.
///
/// Not the same question. Reading one message is one lookup; listing walks
/// hundreds, and on a mailbox with six figures in it Mail charges about a
/// second a message however it is asked. Forty-five seconds bought about forty
/// messages there, which is not enough of somebody's post to call it their
/// unread mail. A morning summary can afford two minutes; a run waiting on a
/// single message cannot, which is why these are two numbers and not one.
const LISTING_TIMEOUT_S: u64 = 120;

/// How far back to walk when looking for unread mail.
///
/// Unread cannot be asked for directly without making Mail build a list of the
/// whole mailbox, which is what breaks at scale, so recent messages are walked
/// instead and the walk stops as soon as enough have been found. Bounded on
/// purpose: a summary of what has just arrived is worth having quickly, and
/// unread post from last year is not what somebody means by "my inbox".
const SCAN_FOR_UNREAD: usize = 200;

/// How many messages one question to Mail covers.
///
/// The walk stops as soon as it has enough, so the size of this decides how
/// much work is done past the point where the answer was already known. Small
/// enough that a mailbox where every message costs Mail real time still gets
/// through the first chunk quickly; large enough that a quiet inbox is one
/// question rather than eight.
const UNREAD_CHUNK: usize = 25;

/// How long the walk may take before it settles for what it has.
///
/// Comfortably inside `TIMEOUT_S`, and that is the point: a walk that stops
/// itself returns what it found and says how far it got, where one that runs
/// into the outer timeout returns nothing at all and used to blame the
/// permission for it. Measured against a real inbox of six figures, where two
/// hundred messages could not be looked at in forty-five seconds however they
/// were asked for.
const WALK_DEADLINE_S: u64 = 100;

/// Three things that must hold, held where they cannot come loose.
///
/// A listing gets longer than a lookup, because they answer different
/// questions. The walk gives up before Errand does, or it never gets to say how
/// far it reached and the whole exercise is back to a timeout with nothing in
/// it. And there is room between the two for the one Apple Event already in
/// flight when the walk's clock runs out.
const _: () = assert!(LISTING_TIMEOUT_S > TIMEOUT_S);
const _: () = assert!(WALK_DEADLINE_S < LISTING_TIMEOUT_S);
const _: () = assert!(LISTING_TIMEOUT_S - WALK_DEADLINE_S >= 15);

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

/// How far back to look when a message is not where the listing said it was.
///
/// The position carried in an id is a starting guess, not a promise: mail
/// arriving between a listing and a read shifts every index down by one. Five
/// hundred covers any plausible amount of new mail in the seconds between the
/// two, and being a fixed number is the whole point -- it is what stops a
/// missing message from turning back into a scan of the entire mailbox.
const RESCAN: usize = 500;

/// Guards on what is interpolated into a script. Nothing legitimate is anywhere
/// near these, and an enormous argument fails deep inside osascript with an
/// error nobody can read.
const MAX_ID_CHARS: usize = 1_200;
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
    /// How many of the most recent messages the walk actually got to look at.
    ///
    /// Counted for the same reason as `unaddressable` below. A walk for unread
    /// mail gives up on its own clock when Mail is slow, and a listing that
    /// came back having examined thirty of the two hundred it meant to is not
    /// the same answer as one that examined all two hundred. Saying "here is
    /// your unread post" off the back of the first would be a lie that reads
    /// exactly like the truth.
    pub checked: usize,
    /// Whether the walk gave up before it had looked everywhere it meant to.
    ///
    /// Said by the walk itself rather than worked out from the count, because
    /// only the walk knows whether it stopped because it was finished or
    /// because it ran out of time.
    pub short: bool,
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

/// Where a message was when it was listed, carried inside the id.
///
/// A message id on its own is not an address. Turning one back into a message
/// means asking Mail to search for it, and `whose message id is ...` builds the
/// whole collection first -- the same thing that made listing an inbox of
/// 191,000 messages fail with -1741 after eight seconds. Listing was taught to
/// walk by index; reading was not, so it went on searching and timed out, and
/// the timeout was reported as a missing permission, sending people to System
/// Settings to fix something that was not broken.
///
/// Carrying the mailbox and the position turns that search into a lookup. The
/// real message id travels too, so the lookup is checked rather than trusted:
/// mail arriving in the seconds between a listing and a read shifts every index.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Locator {
    /// The mailbox it was listed from. Empty means the inbox.
    mailbox: String,
    index: usize,
    message_id: String,
}

/// The tag an Errand-issued id starts with, and the version of its shape.
const LOCATOR_TAG: &str = "E1.";

/// Build the id a listing hands out.
///
/// The mailbox is hex encoded so the id has no delimiter a mailbox name could
/// contain, and the real message id goes last so it may contain anything.
fn locator_id(mailbox: &str, index: usize, message_id: &str) -> String {
    use std::fmt::Write;
    let mut hex = String::with_capacity(mailbox.len() * 2);
    for b in mailbox.as_bytes() {
        let _ = write!(hex, "{b:02x}");
    }
    format!("{LOCATOR_TAG}{index}.{hex}.{message_id}")
}

/// Read one back, or decide it did not come from here.
fn parse_locator(id: &str) -> Option<Locator> {
    let (index, rest) = id.strip_prefix(LOCATOR_TAG)?.split_once('.')?;
    let (hex, message_id) = rest.split_once('.')?;
    let index: usize = index.parse().ok()?;
    if index == 0 || message_id.is_empty() || hex.len() % 2 != 0 {
        return None;
    }
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
        .collect::<Result<_, _>>()
        .ok()?;
    Some(Locator {
        mailbox: String::from_utf8(bytes).ok()?,
        index,
        message_id: message_id.to_string(),
    })
}

/// The part of an id that names the message rather than where it sat.
///
/// An id carries a position so a message can be found quickly, and a position
/// changes every time mail arrives. Anything that asks *which message is this*
/// -- above all the fence that stops one being moved twice -- has to ask this,
/// or the same message listed a minute apart would look like two.
pub fn stable_id(id: &str) -> String {
    parse_locator(id).map_or_else(|| id.to_string(), |l| l.message_id)
}

/// The one line of AppleScript that turns an id into a message.
///
/// Both paths are bounded. An id from somewhere other than a listing -- an
/// older playbook, or a model repeating one from memory -- still gets looked
/// for, but only through recent mail: a search that cannot finish is worse
/// than one that politely does not find it.
fn bind_line(id: &str) -> String {
    match parse_locator(id) {
        Some(l) => format!(
            r#"set m to my findAt("{}", {}, "{}", {RESCAN})"#,
            escape(&l.mailbox),
            l.index,
            escape(&l.message_id)
        ),
        None => format!(r#"set m to my findNear("", "{}", {RESCAN})"#, escape(id)),
    }
}

/// Make a string safe to sit inside an AppleScript literal.
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Run one AppleScript, with a deadline.
async fn osascript(script: &str) -> Result<String, MailError> {
    osascript_within(script, TIMEOUT_S).await
}

/// The same, with its own deadline, because a listing is allowed longer than a
/// lookup.
async fn osascript_within(script: &str, seconds: u64) -> Result<String, MailError> {
    let call = tokio::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .output();

    let out = match tokio::time::timeout(std::time::Duration::from_secs(seconds), call).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(MailError::Machine(format!("macOS could not be asked: {e}"))),
        Err(_) => return Err(why_it_never_answered(seconds).await),
    };

    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).to_string());
    }
    Err(from_stderr(&String::from_utf8_lossy(&out.stderr)))
}

/// A question that never came back, and which of the two reasons it was.
///
/// A timeout used to be reported as the permission prompt nobody can see. That
/// is one of the two things it means, and assuming it is always that one has a
/// cost this module already knew about and paid again: a listing that was
/// simply too big for the mailbox came back as a permission wall, so somebody
/// was sent to press Enable and then to System Settings, for a permission that
/// had been working all afternoon. The wrong half of that answer is worse than
/// no answer, because it is specific and it is actionable and it is a dead end.
///
/// So the cheap question is asked before blaming anybody: may Errand drive Mail
/// at all? It counts mailboxes rather than messages, which is why it answers in
/// a moment on a mailbox where a listing cannot finish. A Mail that answers
/// that has plainly given permission, and the timeout was about size.
async fn why_it_never_answered(seconds: u64) -> MailError {
    use crate::channels::apple::{app_consent, Automation};
    if app_consent(Automation::MailReading).await.is_ok() {
        return MailError::Machine(too_big_to_finish(seconds));
    }
    // Nothing came back from either question, which is the prompt waiting where
    // nobody can see it. Said in the words every other path uses, so the checks
    // that recognise a permission wall recognise this one too.
    MailError::Machine(crate::channels::apple::no_answer(MAIL).to_string())
}

/// What a timeout means when Mail is plainly willing to be driven.
///
/// Kept apart from the question that decides which of the two it was, so the
/// words can be held to in a test: this sentence must never be mistaken for a
/// permission wall by the checks that look for one.
fn too_big_to_finish(seconds: u64) -> String {
    format!(
        "Mail did not finish answering within {seconds} seconds. This is not a permission \
         problem: Errand asked Mail a smaller question straight afterwards and it answered at \
         once. The mailbox is large enough that what was asked for cannot be done in the time. \
         Ask for the most recent messages rather than searching the whole mailbox, or name one \
         smaller mailbox to look in."
    )
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
    // What a mailbox too large to enumerate says. Every path reaches messages
    // by index now -- listing, reading and moving alike -- so this should not
    // happen, but a mailbox can always be bigger than the last one, and a bare
    // number in the run view helps nobody.
    if stderr.contains("-1741") {
        return MailError::Machine(
            "Mail could not hand over that mailbox: it is large enough that asking for its \
             messages all at once fails. Errand asks for them a few at a time, so this usually \
             means the mailbox is unusually large even for that. Try a more specific mailbox."
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
		if boxName is "" then return inbox
		if boxName starts with "acct:" then
			try
				return mailbox "INBOX" of account (text 6 thru -1 of boxName)
			end try
			return missing value
		end if
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

on findAt(boxName, idx, wanted, howManyMost)
	set theBox to my findBox(boxName)
	if theBox is missing value then return missing value
	tell application "Mail"
		try
			set m to message idx of theBox
			if (my flat(message id of m)) is wanted then return m
		end try
	end tell
	return my scanFor(theBox, wanted, howManyMost)
end findAt

on findNear(boxName, wanted, howManyMost)
	set theBox to my findBox(boxName)
	if theBox is missing value then return missing value
	return my scanFor(theBox, wanted, howManyMost)
end findNear

on scanFor(theBox, wanted, howManyMost)
	tell application "Mail"
		set total to (count of messages of theBox)
		set howMany to howManyMost
		if howMany > total then set howMany to total
		repeat with i from 1 to howMany
			try
				set m to message i of theBox
				if (my flat(message id of m)) is wanted then return m
			end try
		end repeat
	end tell
	return missing value
end scanFor

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
/// What to say when the walk stopped before it had looked everywhere.
///
/// None when there is nothing to explain: it found everything it was asked for,
/// or it got through the whole window. Otherwise a sentence naming how far it
/// reached, because "your unread post" and "the unread post in the thirty I
/// could look at" are different claims and only one of them is true.
pub fn stopped_short(found: &Listing, limit: usize, _unread_only: bool) -> Option<String> {
    if !found.short || found.messages.len() >= limit {
        return None;
    }
    Some(format!(
        "Mail was slow enough that only {} messages could be looked at, out of the \
         {SCAN_FOR_UNREAD} of the most recent that this search covers, so there may be older \
         unread post it did not reach. Say so rather than calling this the whole of it.",
        found.checked
    ))
}

/// Turn what the script printed into a listing.
///
/// Split from the call for the same reason the script is: nothing in the suite
/// can reach a real mailbox, so what this module can be held to is the shape of
/// what it asks and what it makes of the answer.
fn parse_listing(reply: &str) -> Listing {
    let mut messages = vec![];
    let mut stamps: Vec<i64> = vec![];
    let mut unaddressable = 0;
    let mut checked = 0usize;
    let mut short = false;
    for line in reply.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if line.trim() == "!no-id" {
            unaddressable += 1;
            continue;
        }
        if let Some(n) = line.trim().strip_prefix("!checked\t") {
            checked = n.trim().parse().unwrap_or(0);
            continue;
        }
        if line.trim() == "!short" {
            short = true;
            continue;
        }
        let mut f = line.split('\t');
        let (Some(from_box), Some(at), Some(id), Some(sender), Some(subject), Some(date)) =
            (f.next(), f.next(), f.next(), f.next(), f.next(), f.next())
        else {
            unaddressable += 1;
            continue;
        };
        let Ok(at) = at.trim().parse::<usize>() else {
            unaddressable += 1;
            continue;
        };
        // The mailbox each message came from, not the one that was asked for:
        // the inbox is walked per account, so "the inbox" is several mailboxes
        // and an id has to name the one it belongs to.
        let stamp: i64 = f.next().and_then(|v| v.trim().parse().ok()).unwrap_or(0);
        stamps.push(stamp);
        messages.push(Summary {
            id: locator_id(from_box.trim(), at, id.trim()),
            sender: sender.trim().to_string(),
            subject: subject.trim().to_string(),
            date: date.trim().to_string(),
            preview: f.next().unwrap_or_default().trim().to_string(),
        });
    }
    // Newest first, across all of them.
    //
    // One mailbox hands its messages over in its own order and Errand leaves
    // that alone. Several mailboxes have no shared order at all, and listing
    // one account's post and then the next one's would put a fortnight-old
    // message above this morning's. So when the walk covered more than one
    // mailbox they are put back in the order a person means by "most recent".
    if !stamps.is_empty() && stamps.iter().any(|&s| s != stamps[0]) {
        let mut together: Vec<(i64, Summary)> = stamps.iter().copied().zip(messages).collect();
        together.sort_by(|a, b| b.0.cmp(&a.0));
        messages = together.into_iter().map(|(_, m)| m).collect();
    }
    Listing {
        messages,
        checked,
        short,
        unaddressable,
    }
}

/// The script one listing runs, built where a test can read it.
///
/// Split out for the reason `no_script_ever_asks_mail_to_search` gives: nothing
/// in the suite can reach a real mailbox, so what this module can be held to is
/// the shape of what it asks Mail, and that is only checkable if it is a string
/// somebody can get hold of.
fn listing_script(box_name: &str, scan: usize, limit: usize, unread_only: bool) -> String {
    // Which mailboxes to walk, and what to call each one in the ids it hands
    // back.
    //
    // Mail's `inbox` is the one it aggregates across every account, and asking
    // it for a message by index is far slower than asking an account's own
    // INBOX: measured on this machine, the aggregate managed twenty-five
    // messages in twenty-five seconds where an account's inbox got through two
    // hundred and found an unread message the aggregate never reached. So the
    // inbox is walked per account. Anything else is the one mailbox asked for.
    //
    // The tag is what goes into every id from that mailbox, so a later read
    // comes back to the same account rather than to whichever account happens
    // to answer to that mailbox name first.
    let boxes = if box_name.is_empty() {
        r#"set boxes to {}
set tags to {}
tell application "Mail"
	repeat with a in accounts
		try
			set end of boxes to mailbox "INBOX" of a
			set end of tags to ("acct:" & (name of a))
		end try
	end repeat
	if (count of boxes) is 0 then
		set boxes to {inbox}
		set tags to {""}
	end if
end tell"#
            .to_string()
    } else {
        format!(
            "set theBox to my findBox(\"{}\")\n\
             if theBox is missing value then return \"!no-mailbox\"\n\
             set boxes to {{theBox}}\n\
             set tags to {{\"{}\"}}",
            escape(box_name),
            escape(box_name)
        )
    };

    // Picking which messages in one mailbox to hand back. Unread asks Mail
    // about a chunk at a time and watches the clock; everything else takes them
    // in the order the mailbox gives them and asks nothing about read status.
    let picks = if unread_only {
        format!(
            "\t\t\trepeat with startAt from 1 to howMany by {UNREAD_CHUNK}\n\
             \t\t\t\tset endAt to startAt + {UNREAD_CHUNK} - 1\n\
             \t\t\t\tif endAt > howMany then set endAt to howMany\n\
             \t\t\t\tset flags to read status of messages startAt thru endAt of theBox\n\
             \t\t\t\trepeat with j from 1 to (count of flags)\n\
             \t\t\t\t\tif item j of flags is false then\n\
             \t\t\t\t\t\tset end of picks to (startAt + j - 1)\n\
             \t\t\t\t\t\tif (count of picks) + found is {limit} then exit repeat\n\
             \t\t\t\t\tend if\n\
             \t\t\t\tend repeat\n\
             \t\t\t\tset checked to checked + (endAt - startAt + 1)\n\
             \t\t\t\tif (count of picks) + found is {limit} then exit repeat\n\
             \t\t\t\tif ((current date) - t0) > {WALK_DEADLINE_S} then\n\
             \t\t\t\t\tif endAt < howMany then set shortWalk to true\n\
             \t\t\t\t\texit repeat\n\
             \t\t\t\tend if\n\
             \t\t\tend repeat"
        )
    } else {
        format!(
            "\t\t\trepeat with i from 1 to howMany\n\
             \t\t\t\tset end of picks to i\n\
             \t\t\t\tset checked to checked + 1\n\
             \t\t\t\tif (count of picks) + found is {limit} then exit repeat\n\
             \t\t\tend repeat"
        )
    };

    format!(
        r#"{handlers}
{boxes}
tell application "Mail"
	set out to ""
	set checked to 0
	set found to 0
	set shortWalk to false
	set t0 to current date
	-- Something to measure dates against, built rather than parsed. A date
	-- written out as text is read in this Mac's own language and format, so
	-- `date "Thursday, 1 January 1970 at 00:00:00"` is a syntax error on a Mac
	-- that does not write dates that way, and the listing failed with one.
	-- Minutes rather than seconds because AppleScript turns integers this large
	-- into reals, and a real reaches Errand as "1.767E+9", which is not a
	-- number it can read back.
	set zeroDate to current date
	set day of zeroDate to 1
	set year of zeroDate to 1970
	set month of zeroDate to January
	set time of zeroDate to 0
	repeat with bi from 1 to (count of boxes)
		if found is {limit} then exit repeat
		if ((current date) - t0) > {WALK_DEADLINE_S} then
			set shortWalk to true
			exit repeat
		end if
		set theBox to item bi of boxes
		set theTag to item bi of tags
		set total to (count of messages of theBox)
		set howMany to {scan}
		if howMany > total then set howMany to total
		set picks to {{}}
		if howMany > 0 then
{picks}
		end if
		repeat with i in picks
			set i to i as integer
			set m to missing value
			try
				set m to message i of theBox
			end try
			if m is missing value then
				set out to out & "!no-id" & linefeed
			else
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
					set stamp to 0
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
						set stamp to ((date received of m) - zeroDate) div 60
					end try
					try
						set pv to my flat(my clip(content of m, {PREVIEW_CHARS}))
					end try
					set found to found + 1
					set out to out & theTag & tab & i & tab & theId & tab & snd & tab & subj & tab & dt & tab & stamp & tab & pv & linefeed
				end if
			end if
		end repeat
	end repeat
	if shortWalk then set out to out & "!short" & linefeed
	return out & "!checked" & tab & checked & linefeed
end tell
"#,
        handlers = HANDLERS,
        boxes = boxes,
        picks = picks,
        scan = scan,
        limit = limit,
    )
}

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

    // An empty name means the inbox, here and in every id this listing hands
    // out, so a message can be found again the same way it was found first.
    let box_name = mailbox.unwrap_or("");
    // Reach for messages one at a time by index, never the whole collection.
    //
    // `messages of theBox` asks Mail to hand over every message in the mailbox
    // before anything is narrowed down. On a real mailbox that is not slow, it
    // is fatal: an inbox of 191,000 messages answered with AppleScript error
    // -1741 after eight seconds. Indexing works at any size, because Mail is
    // asked for one message rather than for a list it has to build first.
    //
    // Unread is the same problem wearing a `whose` clause, so it is done by
    // walking recent messages and stopping early. That means unread mail older
    // than the window is not found, which is the right trade for a summary of
    // what has just arrived: it is bounded and it always answers.
    //
    // Bounded was not enough on its own. Walking asked Mail for one message's
    // read status at a time, and two hundred of those is two hundred Apple
    // Events: on the mailbox above it did not finish inside the timeout, so the
    // first task ever to ask for unread mail failed on the first attempt and
    // every attempt after it. The whole range is asked for in one event now.
    // A range is not a `whose` clause and does not make Mail build the
    // collection; it is the same indexing, asked once instead of two hundred
    // times, and only the few messages that turn out to be unread are then
    // asked about themselves.
    let scan = if unread_only { SCAN_FOR_UNREAD } else { limit };

    let script = listing_script(box_name, scan, limit, unread_only);

    let reply = osascript_within(&script, LISTING_TIMEOUT_S).await?;
    if reply.trim() == "!no-mailbox" {
        return Err(MailError::NoSuchMailbox(
            mailbox.unwrap_or("inbox").to_string(),
        ));
    }

    Ok(parse_listing(&reply))
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
{bind}
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
        bind = bind_line(id),
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
{bind}
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
        bind = bind_line(id),
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
{bind}
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
        bind = bind_line(id),
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

/// The id a rehearsal listing hands out for one of its invented messages.
///
/// Tests used to write this out by hand. That quietly stopped being the id a
/// listing gives the moment ids started carrying a position, so they now ask
/// for it the same way a model gets it.
#[cfg(test)]
pub fn rehearsal_id(nth: usize) -> String {
    rehearsal_messages()[nth - 1].id.clone()
}

fn rehearsal_listing(mailbox: Option<&str>, limit: usize, unread_only: bool) -> Listing {
    // A mailbox nobody has is still a mailbox nobody has, even in a rehearsal:
    // the two invented messages live in the inbox and in "Junk".
    let known = matches!(mailbox, None | Some("Junk") | Some("INBOX") | Some("Inbox"));
    if !known {
        return Listing {
            messages: vec![],
            checked: 0,
            short: false,
            unaddressable: 0,
        };
    }
    let mut messages = rehearsal_messages();
    if unread_only {
        messages.truncate(1);
    }
    messages.truncate(limit);
    Listing {
        // A rehearsal invents what it hands back, so it looked at all of it.
        checked: messages.len(),
        short: false,
        messages,
        unaddressable: 0,
    }
}

fn rehearsal_messages() -> Vec<Summary> {
    vec![
        Summary {
            id: locator_id("", 1, "errand-rehearsal-1@example.invalid"),
            sender: "A Made-Up Sender <nobody@example.invalid>".into(),
            subject: "An invented message, for testing".into(),
            date: "Tuesday 26 August 2026 at 09:00".into(),
            preview: "Errand invented this. There is no such message in anybody's mail.".into(),
        },
        Summary {
            id: locator_id("", 2, "errand-rehearsal-2@example.invalid"),
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
    fn no_script_ever_asks_mail_to_search() {
        // The guard that would have caught this the first time.
        //
        // `whose message id is ...` reads perfectly well and is fatal: Mail
        // builds the whole collection before narrowing it, so on a real inbox
        // it never answers. Listing was fixed and reading was not, and because
        // no test could reach either -- both need a Mac with mail on it -- the
        // suite stayed green while every read of a real mailbox timed out.
        // This asserts on the script instead of on the answer, which needs
        // nothing but a string.
        assert!(
            !HANDLERS.contains("whose"),
            "a handler asks Mail to search; it must walk by index instead"
        );
        for id in ["E1.4.4a756e6b.<abc@example.com>", "<plain@example.com>"] {
            let line = bind_line(id);
            assert!(!line.contains("whose"), "bind_line searches: {line}");
            assert!(line.contains(&RESCAN.to_string()), "unbounded: {line}");
        }
    }

    #[test]
    fn the_unread_walk_asks_about_a_chunk_at_a_time_and_never_one_message() {
        // What broke the first task that ever asked for unread mail: one Apple
        // Event per message, two hundred of them, into a mailbox with six
        // figures in it. It did not finish in forty-five seconds. Asking for
        // the whole window in one event did not finish either, which is why
        // this is a chunk at a time with a clock on it.
        let script = listing_script("set theBox to my findBox(\"\")", 200, 5, true);
        assert!(
            script.contains("read status of messages startAt thru endAt of theBox"),
            "the walk is back to one message at a time: {script}"
        );
        assert!(
            !script.contains("read status of m)"),
            "a per-message read status is back in the walk: {script}"
        );
        // The same rule as everywhere else in this module.
        assert!(!script.contains("whose"), "the listing searches: {script}");
    }

    #[test]
    fn a_listing_that_does_not_want_unread_asks_mail_nothing_about_read_status() {
        // Five most recent is the cheap path and must stay cheap: no bulk
        // fetch, no read status, just the five.
        let script = listing_script("set theBox to my findBox(\"\")", 5, 5, false);
        assert!(!script.contains("read status"), "{script}");
    }

    #[test]
    fn the_unread_walk_gives_up_on_its_own_clock_rather_than_on_errands() {
        // The whole point of the rewrite. A walk that runs into the outer
        // timeout returns nothing and used to blame the permission for it; one
        // that stops itself returns what it found and says how far it reached.
        let script = listing_script("set theBox to my findBox(\"\")", 200, 5, true);
        assert!(script.contains("set t0 to current date"), "{script}");
        assert!(
            script.contains(&format!("((current date) - t0) > {WALK_DEADLINE_S}")),
            "the walk has no deadline of its own: {script}"
        );
        assert!(
            script.contains(&format!("by {UNREAD_CHUNK}")),
            "the walk asks for the whole window in one go again: {script}"
        );
        assert!(script.contains("!checked"), "it never says how far it got");
    }

    #[test]
    fn the_inbox_is_walked_one_account_at_a_time_and_not_through_the_aggregate() {
        // Measured on a real Mac: Mail's aggregate inbox managed twenty-five
        // messages in twenty-five seconds, where one account's own INBOX got
        // through two hundred and turned up an unread message the aggregate
        // never reached. Asking the aggregate for a message by index is the
        // slow thing, so the inbox is several mailboxes now.
        let script = listing_script("", 200, 5, true);
        assert!(
            script.contains("set end of boxes to mailbox \"INBOX\" of a"),
            "the inbox is back to the aggregate: {script}"
        );
        assert!(
            script.contains("set boxes to {inbox}"),
            "nothing to fall back on when no account exposes an INBOX: {script}"
        );
        // Every id says which account it came from, or a later read goes to
        // whichever account answers to that name first.
        assert!(script.contains("(\"acct:\" & (name of a))"), "{script}");
        assert!(
            HANDLERS.contains("mailbox \"INBOX\" of account (text 6 thru -1 of boxName)"),
            "an id that names an account cannot be resolved back"
        );

        // A named mailbox is still just that one.
        let one = listing_script("Junk", 5, 5, false);
        assert!(one.contains("set boxes to {theBox}"), "{one}");
        assert!(!one.contains("repeat with a in accounts"), "{one}");
    }

    #[test]
    fn no_script_ever_writes_a_date_out_as_text() {
        // A date written as text is read back in this Mac's own language and
        // format. `date "Thursday, 1 January 1970 at 00:00:00"` was a syntax
        // error on the first Mac it met, and the listing failed with a message
        // about a broken date on somebody's mail, which was nobody's mail and
        // nothing to do with dates on messages at all.
        let script = listing_script("", 200, 5, true);
        // Comments may name the trap; only lines that run may not fall into it.
        let runs: String = script
            .lines()
            .filter(|l| !l.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !runs.contains("date \""),
            "a date is being parsed from text: {runs}"
        );
        assert!(script.contains("set year of zeroDate to 1970"), "{script}");
        // Minutes, because AppleScript hands back seconds this large as a real
        // and a real arrives as "1.767E+9", which parses as nothing.
        assert!(script.contains("div 60"), "{script}");
    }

    #[test]
    fn messages_from_several_accounts_come_back_newest_first() {
        // One mailbox has its own order and Errand leaves it alone. Several
        // have no shared order, and handing over one account's post and then
        // the next one's puts a fortnight-old message above this morning's.
        let line = |tag: &str, stamp: &str, subject: &str| {
            format!("{tag}\t1\tid-{subject}\tsomebody\t{subject}\twhenever\t{stamp}\ta preview")
        };
        let reply = format!(
            "{}\n{}\n!checked\t400\n",
            line("acct:One", "1735689600", "the older one"),
            line("acct:Two", "1767225600", "the newer one"),
        );
        let got = parse_listing(&reply);
        assert_eq!(
            got.messages[0].subject, "the newer one",
            "{:?}",
            got.messages
        );
        assert_eq!(got.messages[1].subject, "the older one");
        // And each id remembers the account it came from.
        assert_eq!(
            parse_locator(&got.messages[0].id).unwrap().mailbox,
            "acct:Two"
        );
    }

    #[test]
    fn a_walk_that_was_cut_short_says_so_and_one_that_was_not_says_nothing() {
        let two = |checked: usize, short: bool| Listing {
            messages: vec![rehearsal_messages()[0].clone()],
            checked,
            short,
            unaddressable: 0,
        };
        // Stopped at thirty of two hundred, with fewer than asked for: worth
        // saying, because the answer is about thirty messages and not two
        // hundred.
        let said = stopped_short(&two(30, true), 5, true).expect("a short walk says so");
        assert!(said.contains("30"), "{said}");
        assert!(
            said.contains("Say so"),
            "the agent is told to pass it on: {said}"
        );

        // Got through the whole window: nothing to explain.
        assert_eq!(stopped_short(&two(SCAN_FOR_UNREAD, false), 5, true), None);
        // Found everything it was asked for: the window is beside the point.
        assert_eq!(stopped_short(&two(30, true), 1, true), None);
    }

    #[test]
    fn a_mailbox_too_big_to_finish_is_not_reported_as_a_permission_problem() {
        // The half of this that cost a real afternoon. A timeout was said in
        // the words of a refused permission, so the person was sent to press
        // Enable and then into System Settings, for a permission that had been
        // working all day.
        let said = too_big_to_finish(LISTING_TIMEOUT_S);
        assert!(
            !crate::channels::apple::is_permission_block(&said),
            "a slow mailbox still reads as a permission wall: {said}"
        );
        assert!(said.contains("not a permission problem"), "{said}");
        assert!(said.contains("most recent"), "it says what to do: {said}");

        // And the other half still is one, so everything that reacts to a wall
        // goes on reacting to it.
        assert!(crate::channels::apple::is_permission_block(
            &crate::channels::apple::no_answer(MAIL).to_string()
        ));
    }

    #[test]
    fn an_id_says_where_the_message_was() {
        let id = locator_id("Junk", 4, "<abc@example.com>");
        let back = parse_locator(&id).expect("an id we made is one we can read");
        assert_eq!(back.mailbox, "Junk");
        assert_eq!(back.index, 4);
        assert_eq!(back.message_id, "<abc@example.com>");
        // The inbox is the empty name, and survives the round trip as one.
        assert_eq!(
            parse_locator(&locator_id("", 1, "<x@y>")).unwrap().mailbox,
            ""
        );
    }

    #[test]
    fn a_mailbox_name_survives_being_carried_in_an_id() {
        // Hex is used precisely so a name may contain the delimiter, a quote,
        // or anything else somebody has actually called a mailbox.
        for name in [
            "Junk",
            "Work.Archive.2019",
            r#"Odd" name"#,
            "Ablage – alt",
            "",
        ] {
            let id = locator_id(name, 7, "<m@example.com>");
            assert_eq!(parse_locator(&id).unwrap().mailbox, name, "lost: {name:?}");
        }
    }

    #[test]
    fn an_id_from_somewhere_else_is_still_looked_for() {
        // A playbook written before ids carried a position, or a model
        // repeating one from memory. It must not be refused, and must not
        // become a search either.
        for id in [
            "<older@example.com>",
            "E1.notanumber.4a.<x@y>",
            "E1.0.4a.<x@y>",
            "E1.4.odd.<x@y>",
            "E1.4.4a756e6b.",
        ] {
            assert!(parse_locator(id).is_none(), "read as a locator: {id}");
            assert!(bind_line(id).contains("findNear"), "not looked for: {id}");
        }
    }

    #[test]
    fn a_real_id_fits_the_length_guard() {
        // The guard exists to stop an enormous argument reaching osascript. It
        // now has to leave room for a hex mailbox name and the id itself.
        let id = locator_id(&"m".repeat(MAX_MAILBOX_CHARS), 999_999, &"<a@b>".repeat(20));
        assert!(
            check_id(&id).is_ok(),
            "an id this tool hands out is refused by its own guard: {} chars",
            id.chars().count()
        );
    }

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

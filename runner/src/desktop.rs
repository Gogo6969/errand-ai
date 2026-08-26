//! Putting an answer where a person will actually see it.
//!
//! A run that found the right answer and left it in a journal nobody opens has
//! not really done the job. So there are three ways out of the machine: a note
//! in Apple Notes, a text file in a folder of the person's own, and opening
//! something in front of them on their own screen.
//!
//! Everything here touches the real Mac, so the same three things hold
//! throughout. osascript gets a hard deadline, because it blocks for ever on a
//! consent prompt that appears where nobody is looking, which is the shape of
//! the bug that once wedged the whole daemon. Everything interpolated into a
//! script is escaped, because a note body is text somebody else wrote. And
//! `ERRAND_APPLE_DRY` turns the module into a rehearsal, so the test suite can
//! prove the rules without anyone's real Notes gaining a test note.
//!
//! What is deliberately not here is the policy: whether the run is a rehearsal,
//! and whether a site is on the task's list, are decided in `mcp`, alongside
//! every other rule the agent is held to.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// Long enough for Notes to start from cold, short enough that a wedged run is
/// still a run that ends.
const TIMEOUT_S: u64 = 30;

/// The most a note may carry.
///
/// The whole script, body included, goes to osascript as one argument, so an
/// unbounded body fails somewhere deep with an error nobody can read. This is
/// far more than a daily summary needs.
const MAX_NOTE_CHARS: usize = 50_000;

/// The most a saved file may carry. A run writes summaries, not archives.
const MAX_FILE_BYTES: usize = 1_000_000;

/// Long enough to be descriptive, short enough for every filesystem and for the
/// Notes title bar.
const MAX_NAME_CHARS: usize = 120;

/// The app whose name goes in a refusal about a note. The switch a person has
/// to find is labelled with this, so nothing here may call it "the app".
pub const NOTES: &str = "Apple Notes";

/// True when nothing may touch the real machine.
///
/// The tests set this. Without it, verifying that a bad file name is refused
/// would mean opening TextEdit on whoever ran `cargo test`.
pub fn dry() -> bool {
    std::env::var("ERRAND_APPLE_DRY").is_ok()
}

tokio::task_local! {
    /// A Mac that will not co-operate, for the length of one test.
    ///
    /// The refusal path is the one that ended a real run with a shrug, so it
    /// has to be provable without a real Mac and without a real permission
    /// prompt. Deliberately a task-local rather than an environment variable:
    /// the suite runs its tests side by side in one process, and a variable set
    /// by one of them would be read by all of them, which is how a rehearsal
    /// starts failing tests that never asked for one.
    ///
    /// It carries what osascript would have printed, so a rehearsed refusal
    /// goes through the same translation as a real one rather than through a
    /// second set of words that could drift.
    pub static PRETEND_MACOS_SAID: String;
}

/// What macOS is pretending to have said, when a test is rehearsing a refusal.
pub fn rehearsed_refusal() -> Option<String> {
    PRETEND_MACOS_SAID.try_with(|said| said.clone()).ok()
}

/// Turn what osascript printed into something a person can act on.
///
/// Shared by the real path and by a rehearsal so both produce one set of words.
fn note_failure(stderr: &str) -> anyhow::Error {
    // -1728 is Notes saying it has no account to put a note in. The shared
    // translation reads it as a missing recipient, which is right for mail and
    // meaningless here, so this one is answered where the question was asked.
    if stderr.contains("-1728") {
        return anyhow::anyhow!(
            "the Notes app has no account set up, so there is nowhere to put a note. Open Notes \
             on the Mac once and sign in, then this will work."
        );
    }
    // Refused consent and an app that is not running mean the same thing
    // wherever they happen, and are already translated once for the mail and
    // messages channels. A second copy of those codes here would be a second
    // thing to keep right.
    anyhow::anyhow!("{}", crate::channels::apple::translate(NOTES, stderr))
}

/// Run one AppleScript, with a deadline.
async fn osascript(script: &str) -> Result<String> {
    let call = tokio::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .output();

    let out = match tokio::time::timeout(std::time::Duration::from_secs(TIMEOUT_S), call).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => bail!("macOS could not be asked to do that: {e}"),
        // Not a hang to wait out: this is the prompt nobody can see, and it is
        // the same problem as an outright refusal, so it is said in the same
        // words and recognised by the same check.
        Err(_) => bail!("{}", crate::channels::apple::no_answer(NOTES)),
    };

    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    Err(note_failure(&String::from_utf8_lossy(&out.stderr)))
}

/// Make a string safe to sit inside an AppleScript literal.
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Turn plain text into the HTML a note body actually is.
///
/// Two reasons, and both are visible to the person. Without it the whole
/// summary arrives as one run-on paragraph. And a `<` in something the agent
/// read off a page would become markup instead of the character it read.
fn as_html(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let safe = line
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        if safe.trim().is_empty() {
            out.push_str("<div><br></div>");
        } else {
            out.push_str("<div>");
            out.push_str(&safe);
            out.push_str("</div>");
        }
    }
    out
}

/// One more entry to add to the bottom of a note, with the day it was written.
///
/// The point of appending is a record over time, and an entry with no date on
/// it tells the reader nothing about when it was true. Local time, not UTC:
/// this is read by a person, not matched by a machine.
fn dated_entry(body: &str) -> String {
    let stamp = chrono::Local::now().format("%-d %B %Y, %H:%M").to_string();
    format!("<div><br></div>{}{}", as_html(&stamp), as_html(body))
}

/// What happened to the note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteWrite {
    Created,
    Appended,
}

/// Write into Apple Notes, either as a new note or added to one already there.
///
/// Appending by title is the whole point of the option: a week of daily
/// summaries in one note is something a person reads, and seven notes with the
/// same name is something they delete.
pub async fn save_note(title: &str, body: &str, append: bool) -> Result<NoteWrite> {
    let title = title.trim();
    if title.is_empty() {
        bail!("A note needs a title, or the person has nothing to look for in Notes.");
    }
    if title.chars().count() > MAX_NAME_CHARS {
        bail!(
            "That title is {} characters long and a note title may be at most {MAX_NAME_CHARS}. \
             Give it a short name and put the detail in the body.",
            title.chars().count()
        );
    }
    if body.trim().is_empty() {
        bail!("There is nothing in that note, so there is nothing to write. Put what you found in the body.");
    }
    if body.chars().count() > MAX_NOTE_CHARS {
        bail!(
            "That note is {} characters long and the most a note may hold is {MAX_NOTE_CHARS}. \
             Nothing was written. Summarise what matters and write that instead.",
            body.chars().count()
        );
    }

    if dry() {
        // A rehearsal answers the way this Mac would, which includes answering
        // the way a Mac that was never given permission answers.
        return match rehearsed_refusal() {
            Some(stderr) => Err(note_failure(&stderr)),
            None => Ok(if append {
                NoteWrite::Appended
            } else {
                NoteWrite::Created
            }),
        };
    }

    // The title also opens the body, because Notes shows the first line as the
    // note's heading whatever the name property says.
    let fresh = format!("{}{}", as_html(title), as_html(body));
    let script = if append {
        let addition = dated_entry(body);
        format!(
            r#"tell application "Notes"
                tell account 1
                    set found to (every note whose name is "{title}")
                    if (count of found) is 0 then
                        make new note with properties {{name:"{title}", body:"{fresh}"}}
                        return "created"
                    else
                        set n to item 1 of found
                        set body of n to ((body of n) & "{addition}")
                        return "appended"
                    end if
                end tell
            end tell"#,
            title = escape(title),
            fresh = escape(&fresh),
            addition = escape(&addition),
        )
    } else {
        format!(
            r#"tell application "Notes"
                tell account 1
                    make new note with properties {{name:"{title}", body:"{fresh}"}}
                    return "created"
                end tell
            end tell"#,
            title = escape(title),
            fresh = escape(&fresh),
        )
    };

    let said = osascript(&script).await?;
    Ok(if said == "appended" {
        NoteWrite::Appended
    } else {
        NoteWrite::Created
    })
}

/// The one folder a run may write a file into.
pub fn files_dir() -> Result<PathBuf> {
    errand_core::paths::files_dir()
}

/// Turn what the agent asked to call a file into something that can only be a
/// name.
///
/// The agent supplies a name, never a path, and this is what makes that true
/// rather than merely requested. A slash, a `..`, or a leading dot are each a
/// way of writing somewhere other than the folder the person looks in: two of
/// them reach the database sitting a level up, and the third writes a file
/// Finder does not show. All three are refused with a sentence saying what to
/// type instead, because the agent can act on that and cannot act on "denied".
///
/// A name with no extension gains `.txt`, so double-clicking it opens something
/// rather than asking the person which app to use.
pub fn safe_name(name: &str) -> Result<String> {
    let typed = name.trim();
    if typed.is_empty() {
        bail!("A file needs a name. Something like bitcoin-news.txt.");
    }
    if typed.chars().count() > MAX_NAME_CHARS {
        bail!(
            "That name is {} characters long and a file name may be at most {MAX_NAME_CHARS}. \
             Give it a short name.",
            typed.chars().count()
        );
    }
    if typed.contains('/') || typed.contains('\\') || typed.contains(':') {
        bail!(
            "'{typed}' is a path, not a name. Errand keeps these files in one folder of its own \
             and you cannot choose where they go, so give a plain name with nothing separating \
             it, such as bitcoin-news.txt."
        );
    }
    if typed.contains("..") {
        bail!(
            "'{typed}' has two dots together in it, which is how a name climbs out of the folder \
             it belongs in, so it has not been saved. Use a name with single dots, such as \
             bitcoin-news.txt."
        );
    }
    if typed.starts_with('.') {
        bail!(
            "'{typed}' starts with a dot, which on a Mac means a file the person will never see \
             in Finder. Start the name with a letter, such as bitcoin-news.txt."
        );
    }
    if typed.chars().any(char::is_control) {
        bail!(
            "That name contains a character that is not printable, so it has not been saved. Use \
             ordinary letters, numbers, spaces, dots and hyphens."
        );
    }

    if Path::new(typed).extension().is_none() {
        return Ok(format!("{typed}.txt"));
    }
    Ok(typed.to_string())
}

/// Write a text file into the Errand Files folder, and say where it went.
pub async fn save_file(name: &str, content: &str) -> Result<PathBuf> {
    let name = safe_name(name)?;
    if content.len() > MAX_FILE_BYTES {
        bail!(
            "That file is {} bytes and the most one may hold is {MAX_FILE_BYTES}. Nothing was \
             saved. Write the part that matters.",
            content.len()
        );
    }

    let dir = files_dir()?;
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("making the folder {}", dir.display()))?;
    let path = dir.join(&name);

    // Belt and braces over `safe_name`. A name with nothing separating it
    // cannot land anywhere but this folder, so this can only fire if the rule
    // above is ever loosened, which is exactly when it needs to fire.
    if path.parent() != Some(dir.as_path()) {
        bail!("'{name}' would not land in the Errand Files folder, so nothing was saved.");
    }

    tokio::fs::write(&path, content)
        .await
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Check an app is named rather than pointed at.
///
/// `open -a` takes a name, and a name is all this accepts: no slashes means no
/// pointing it at a bundle downloaded a minute ago somewhere else on disk.
pub fn safe_app_name(name: &str) -> Result<String> {
    let typed = name.trim();
    if typed.is_empty() {
        bail!("Say which app to open, by its name, such as TextEdit.");
    }
    if typed.chars().count() > 60 {
        bail!("'{typed}' is too long to be an app's name. Give the name as it appears in your Applications folder.");
    }
    if !typed
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '.' | '-' | '_' | '+'))
    {
        bail!(
            "'{typed}' is not a name Errand will open. Give the app's name exactly as it appears \
             in the Applications folder, such as TextEdit or Safari, with no path in it."
        );
    }
    Ok(typed.to_string())
}

/// Hand something to macOS to open, the way a double-click does.
async fn run_open(args: &[&str]) -> Result<()> {
    if dry() {
        return Ok(());
    }
    let call = tokio::process::Command::new("/usr/bin/open")
        .args(args)
        .output();
    let out = match tokio::time::timeout(std::time::Duration::from_secs(TIMEOUT_S), call).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => bail!("macOS could not be asked to open that: {e}"),
        Err(_) => bail!(
            "macOS did not answer within {TIMEOUT_S} seconds, so nothing was opened. The Mac may \
             be showing a prompt where nobody can see it."
        ),
    };
    if out.status.success() {
        return Ok(());
    }
    let why: String = String::from_utf8_lossy(&out.stderr)
        .lines()
        .next()
        .unwrap_or("macOS did not say why")
        .chars()
        .take(200)
        .collect();
    bail!("{}", why.trim())
}

/// Open a web address in the person's own browser.
///
/// Whether the address is allowed is not decided here: `mcp` checks it against
/// the task's list first, exactly as it does for a navigation.
pub async fn open_url(url: &str) -> Result<()> {
    run_open(&[url]).await
}

/// Open a saved file in whatever the Mac uses for it.
pub async fn open_file(path: &Path) -> Result<()> {
    run_open(&[&path.to_string_lossy()]).await
}

/// Bring an app to the front, starting it if it is not running.
/// Bring one note to the front in Apple Notes.
///
/// `open` cannot address a note, so this asks Notes itself. Matching by title
/// is what the person sees and what save_note wrote, and a title that no longer
/// matches simply opens the app, which is a better answer than an error about a
/// note somebody has since renamed.
pub async fn open_note(title: &str) -> Result<()> {
    let script = format!(
        r#"tell application "Notes"
	activate
	try
		show (first note whose name is "{title}")
	end try
end tell"#,
        title = title.replace('\\', "\\\\").replace('"', "\\\"")
    );
    osascript(&script).await.map(|_| ())
}

pub async fn open_app(name: &str) -> Result<()> {
    run_open(&["-a", name]).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_with_a_folder_in_it_is_refused_and_says_what_to_type_instead() {
        for attempt in [
            "notes/bitcoin.txt",
            "..\\bitcoin.txt",
            "Macintosh HD:bitcoin",
        ] {
            let why = safe_name(attempt)
                .expect_err("a path is not a name")
                .to_string();
            assert!(
                why.contains("plain name"),
                "the refusal has to say what to type instead: {why}"
            );
        }
    }

    #[test]
    fn a_name_that_climbs_out_of_the_folder_is_refused() {
        assert!(safe_name("../errand.db").is_err());
        assert!(safe_name("..").is_err());
        assert!(safe_name("bitcoin..txt").is_err());
    }

    #[test]
    fn a_file_the_person_would_never_see_in_finder_is_refused() {
        assert!(safe_name(".hidden.txt").is_err());
        assert!(safe_name("  .zshrc").is_err());
    }

    #[test]
    fn an_ordinary_name_is_kept_and_one_with_no_ending_can_still_be_opened() {
        assert_eq!(safe_name("bitcoin-news.txt").unwrap(), "bitcoin-news.txt");
        assert_eq!(safe_name(" Monday notes ").unwrap(), "Monday notes.txt");
        assert_eq!(safe_name("prices.csv").unwrap(), "prices.csv");
    }

    #[test]
    fn an_app_can_be_named_but_never_pointed_at() {
        assert_eq!(safe_app_name("TextEdit").unwrap(), "TextEdit");
        assert_eq!(safe_app_name(" Google Chrome ").unwrap(), "Google Chrome");
        assert!(safe_app_name("/Volumes/USB/Thing.app").is_err());
        assert!(safe_app_name("").is_err());
    }

    #[test]
    fn a_note_body_arrives_as_text_rather_than_as_markup() {
        let html = as_html("BTC <b>up</b> 3%\n\nsource: news & views");
        assert!(html.contains("&lt;b&gt;up&lt;/b&gt;"), "{html}");
        assert!(html.contains("news &amp; views"), "{html}");
        // The blank line is kept, because a wall of text is what makes a note
        // unreadable.
        assert!(html.contains("<div><br></div>"), "{html}");
    }

    #[test]
    fn each_day_added_to_a_note_carries_the_day_it_was_written() {
        let now = chrono::Local::now();
        let entry = dated_entry("BTC is up 3%.");
        assert!(
            entry.contains(&now.format("%B").to_string())
                && entry.contains(&now.format("%Y").to_string()),
            "a week of entries with no dates on them is not a record: {entry}"
        );
        assert!(
            entry.starts_with("<div><br></div>"),
            "today's entry has to start clear of yesterday's: {entry}"
        );
        assert!(entry.contains("BTC is up 3%."), "{entry}");
    }

    #[test]
    fn quotes_in_a_note_cannot_break_out_of_the_script() {
        assert_eq!(escape(r#"say "hi" \ bye"#), r#"say \"hi\" \\ bye"#);
    }

    #[tokio::test]
    async fn a_note_with_nothing_in_it_is_refused_rather_than_written_empty() {
        std::env::set_var("ERRAND_APPLE_DRY", "1");
        assert!(save_note("Bitcoin", "   ", false).await.is_err());
        assert!(save_note("  ", "something", false).await.is_err());
    }
}

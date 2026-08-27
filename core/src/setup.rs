//! What Errand can work out for itself from the way a task is written.
//!
//! A person writing "look at my inbox every morning at 7 and tell me what
//! matters" has already said when it runs and what it needs to touch. Making
//! them then say it again in five panels is asking for the same work twice,
//! and the panels were what they saw before they saw any result.
//!
//! Two rules hold this together and neither bends:
//!
//! 1. Anything the caller supplied wins. Inference fills gaps; it never
//!    overrules a person who did say what they wanted.
//! 2. A prohibition always beats a grant. "Clean my mailbox but never delete
//!    anything" is one sentence that asks for filing and forbids deletion, and
//!    reading only the first half of it is how a task deletes somebody's post.
//!
//! What is deliberately never inferred is listed in [`never_inferred`]. The
//! short version: nothing that lets a task spend money, message a person, or
//! sign in as anybody. Those are decisions a person makes once, on purpose.

use serde_json::json;

/// Everything worked out from one description.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Setup {
    /// Sites named in the text, tidied the way the allowlist stores them.
    pub domains: Vec<String>,
    /// Whether the task plainly concerns the person's mail, and how far.
    pub mail: Option<Mail>,
    /// A schedule, as the JSON a `ScheduleSpec` parses.
    pub schedule: Option<serde_json::Value>,
    /// What was decided and the words that decided it, for showing back.
    pub notes: Vec<Note>,
    /// What the description rules out. Kept because it is the more important
    /// half: a grant withheld is worth saying out loud.
    pub forbids: Vec<Forbid>,
}

/// How far into the mailbox a task is allowed to reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mail {
    /// May it move messages between mailboxes. Never inferred from a task that
    /// only asks to read, and never from one that forbids it.
    pub may_file: bool,
}

/// One line of "here is what I set up", written for a person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// What was set.
    pub what: String,
    /// The words in their description that decided it.
    pub because: String,
}

/// Something the description rules out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Forbid {
    /// Moving, filing, archiving or deleting mail.
    MovingMail,
    /// Opening the body of a message rather than its preview.
    OpeningBodies,
    /// Sending anything to anybody.
    Messaging,
    /// Buying, ordering or paying.
    Spending,
}

impl Forbid {
    /// How to say it back to the person who wrote it.
    pub fn plainly(self) -> &'static str {
        match self {
            Forbid::MovingMail => "not to move, file or delete any mail",
            Forbid::OpeningBodies => "not to open message bodies",
            Forbid::Messaging => "not to send anything to anybody",
            Forbid::Spending => "not to buy or pay for anything",
        }
    }
}

/// The settings Errand will never decide for somebody, and why.
///
/// Here as a function rather than a comment so it can be shown on a screen and
/// so a later change that starts inferring one of these has to delete a line
/// that says out loud why it should not.
pub fn never_inferred() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "Signing in anywhere",
            "A login is typed once, by the person, and kept in the Mac's keychain.",
        ),
        (
            "Messaging anybody",
            "Who a task may write to is a permission of its own, so that one task cannot tell \
             somebody about another task's work.",
        ),
        (
            "Buying or paying",
            "Money leaving an account is not something to guess at from a sentence.",
        ),
        (
            "Being told when it fails",
            "Notifications are the only thing standing between a task that quietly breaks at \
             three in the morning and somebody noticing, so they are not switched on or off by \
             a reading of the words.",
        ),
        (
            "Which AI carries it out",
            "The default is whatever the AI screen says. A task that should use a particular \
             model is a deliberate choice, usually about privacy.",
        ),
    ]
}

/// Read a description and work out what the task needs.
pub fn infer(description: &str) -> Setup {
    let text = description.to_lowercase();
    let mut s = Setup {
        forbids: forbidden(&text),
        ..Default::default()
    };

    // Sites first, because they are the least dangerous thing to be wrong
    // about: a site nobody needs is unused, and a site that is missing is a
    // refusal the person can read.
    s.domains = sites_in(description);
    if !s.domains.is_empty() {
        s.notes.push(Note {
            what: format!("It may open {}", join_plainly(&s.domains)),
            because: "you named it in the task".into(),
        });
    }

    if let Some(mail) = mail_wanted(&text, &s.forbids) {
        s.mail = Some(mail);
        s.notes.push(Note {
            what: if mail.may_file {
                "It may read your mail and move messages between mailboxes".into()
            } else {
                "It may read your mail, and cannot move or delete anything".into()
            },
            because: "the task is about your mailbox".into(),
        });
    }

    if let Some((expr, said)) = cron_in(&text) {
        s.schedule = Some(json!({
            "kind": "cron",
            "expr": expr,
            "tz": local_tz(),
        }));
        s.notes.push(Note {
            what: "It runs on a schedule".into(),
            because: format!("you wrote {said:?}"),
        });
    }

    for f in &s.forbids {
        s.notes.push(Note {
            what: format!("It is held {}", f.plainly()),
            because: "you said so in the task".into(),
        });
    }
    s
}

// ------------------------------------------------------------------ sites --

/// Websites named in the text, in the order they appear.
///
/// Deliberately narrow. It offers what is written down and never guesses that
/// "CDV Software" means a domain, because a guess that lands on the wrong site
/// is a task pointed at a stranger's server, possibly carrying a login.
fn sites_in(text: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for raw in text.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | '<' | '>' | ',')) {
        let token =
            raw.trim_matches(|c: char| matches!(c, '.' | '"' | '\'' | ';' | ':' | '!' | '?'));
        if token.is_empty() {
            continue;
        }
        let looks_like_a_site = token.starts_with("http://")
            || token.starts_with("https://")
            || (token.contains('.') && has_known_ending(token));
        if !looks_like_a_site {
            continue;
        }
        if let Ok(host) = crate::domains::normalize_domain(token) {
            if !out.contains(&host) {
                out.push(host);
            }
        }
    }
    out
}

/// Endings common enough to recognise a bare host by, without a scheme.
///
/// A list rather than "any dot" because ordinary prose is full of dots: "e.g."
/// and "Inc." are not websites, and a task pointed at one would be a task that
/// cannot browse anywhere it needs to.
const ENDINGS: &[&str] = &[
    "com", "org", "net", "io", "dev", "app", "co", "ai", "uk", "de", "at", "ch", "fr", "es", "it",
    "nl", "se", "no", "eu", "info", "biz", "me", "tv", "shop", "club", "online", "site", "store",
];

fn has_known_ending(token: &str) -> bool {
    let host = token.split(['/', '?', '#']).next().unwrap_or(token);
    let host = host.split(':').next().unwrap_or(host);
    match host.rsplit('.').next() {
        Some(end) => ENDINGS.contains(&end.to_ascii_lowercase().as_str()),
        None => false,
    }
}

fn join_plainly(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

// ------------------------------------------------------------------- mail --

/// Words that mean somebody's own post, and not iMessage or a web form.
const MAIL_WORDS: &[&str] = &[
    "inbox",
    "mailbox",
    "my mail",
    "my email",
    "my e-mail",
    "my emails",
    "email",
    "emails",
    "e-mail",
    "junk",
    "spam",
];

/// Words that ask for a message to be moved somewhere.
const FILING_WORDS: &[&str] = &[
    "move", "file ", "filing", "archive", "delete", "junk", "spam", "tidy", "clean",
];

fn mail_wanted(text: &str, forbids: &[Forbid]) -> Option<Mail> {
    if !MAIL_WORDS.iter().any(|w| text.contains(w)) {
        return None;
    }
    // A prohibition beats the ask, always, and this is where that matters most:
    // "clean my mailbox but never delete anything" asks for filing in its first
    // half and forbids it in its second.
    let may_file =
        !forbids.contains(&Forbid::MovingMail) && FILING_WORDS.iter().any(|w| text.contains(w));
    Some(Mail { may_file })
}

// ------------------------------------------------------------- what it may not --

/// Ways of saying no, as they are actually written.
const NEGATIONS: &[&str] = &[
    "never ",
    "do not ",
    "don't ",
    "dont ",
    "must not ",
    "no need to ",
    "without ",
    "not to ",
];

/// How far after a "never" to keep looking for what is being refused.
///
/// Long enough for a real sentence, because people write "never move, file or
/// delete anything" and the last verb is a long way from the first word.
const REACH: usize = 90;

fn forbidden(text: &str) -> Vec<Forbid> {
    let mut out: Vec<Forbid> = vec![];
    let mut push = |f: Forbid| {
        if !out.contains(&f) {
            out.push(f);
        }
    };
    for neg in NEGATIONS {
        let mut from = 0usize;
        while let Some(found) = text[from..].find(neg) {
            let start = from + found + neg.len();
            let end = (start + REACH).min(text.len());
            // Byte slicing on a lowercased ASCII-ish description is fine, but a
            // description can hold anything, so the boundary is checked.
            let mut end = end;
            while end > start && !text.is_char_boundary(end) {
                end -= 1;
            }
            let clause = &text[start..end];
            let clause = clause.split(['.', ';', '\n']).next().unwrap_or(clause);

            if ["move", "file", "delete", "archive", "junk", "bin", "trash"]
                .iter()
                .any(|v| clause.contains(v))
            {
                push(Forbid::MovingMail);
            }
            if clause.contains("open") && (clause.contains("body") || clause.contains("bodies")) {
                push(Forbid::OpeningBodies);
            }
            // "message" on its own is too loose to use here. "Do not open
            // message bodies" is about reading, and matching it produced a
            // task that told the person it was held back from writing to
            // anybody, which is not what they said and not what happened.
            if [
                "send",
                "reply",
                "write to",
                "text ",
                "tell anyone",
                "email anyone",
            ]
            .iter()
            .any(|v| clause.contains(v))
                || [
                    "message anyone",
                    "message anybody",
                    "message them",
                    "message my",
                ]
                .iter()
                .any(|v| clause.contains(v))
            {
                push(Forbid::Messaging);
            }
            if [
                "buy",
                "order",
                "pay",
                "purchase",
                "spend",
                "checkout",
                "check out",
            ]
            .iter()
            .any(|v| clause.contains(v))
            {
                push(Forbid::Spending);
            }
            from = start;
        }
    }
    out.sort();
    out
}

// --------------------------------------------------------------- when it runs --

/// Weekday names, in cron's own order, so the index is the cron field.
const DAYS: &[(&str, u32)] = &[
    ("sunday", 0),
    ("monday", 1),
    ("tuesday", 2),
    ("wednesday", 3),
    ("thursday", 4),
    ("friday", 5),
    ("saturday", 6),
];

/// A cron expression and the words it came from, or nothing.
///
/// Six fields, seconds first, which is what this engine reads. An expression
/// copied from anywhere else has five and would mean something quite different.
fn cron_in(text: &str) -> Option<(String, String)> {
    let time = time_of_day(text);

    for (name, dow) in DAYS {
        if text.contains(name) {
            let (h, m) = time.unwrap_or((9, 0));
            return Some((format!("0 {m} {h} * * {dow}"), format!("every {name}")));
        }
    }

    if text.contains("every hour") || text.contains("hourly") {
        return Some(("0 0 * * * *".into(), "every hour".into()));
    }

    let daily = [
        "every morning",
        "each morning",
        "every day",
        "each day",
        "daily",
        "every evening",
        "every night",
        "each night",
    ]
    .iter()
    .find(|w| text.contains(**w))
    .copied();

    match (daily, time) {
        (Some(said), _) => {
            let (h, m) = time.unwrap_or_else(|| default_hour_for(said));
            Some((format!("0 {m} {h} * * *"), said.to_string()))
        }
        // A bare time with no cadence still means every day: "tell me at 7am"
        // is not a request to be told once.
        (None, Some((h, m))) => Some((format!("0 {m} {h} * * *"), format!("at {h:02}:{m:02}"))),
        (None, None) => None,
    }
}

fn default_hour_for(said: &str) -> (u32, u32) {
    if said.contains("morning") {
        (8, 0)
    } else if said.contains("evening") {
        (18, 0)
    } else if said.contains("night") {
        (22, 0)
    } else {
        (9, 0)
    }
}

/// A clock time written the way people write one: 7am, 7 am, 07:00, 7:30pm.
fn time_of_day(text: &str) -> Option<(u32, u32)> {
    let b = text.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        // A digit that is part of a longer word (an address, a version) is not
        // a time.
        if i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'.' || b[i - 1] == b':') {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let hour: u32 = text[start..i].parse().ok()?;
        if i - start > 2 || hour > 24 {
            continue;
        }
        let mut minute = 0u32;
        if i < b.len() && b[i] == b':' {
            let m0 = i + 1;
            let mut m1 = m0;
            while m1 < b.len() && b[m1].is_ascii_digit() {
                m1 += 1;
            }
            if m1 > m0 {
                minute = text[m0..m1].parse().unwrap_or(0);
                i = m1;
            }
        }
        let rest = text[i..].trim_start();
        let pm = rest.starts_with("pm") || rest.starts_with("p.m");
        let am = rest.starts_with("am") || rest.starts_with("a.m");
        // A bare number is only a time when the sentence was already about
        // clocks: "5 messages" is not five o'clock.
        let clocklike = am || pm || text[..start].ends_with("at ") || minute > 0;
        if !clocklike || minute > 59 {
            continue;
        }
        let hour = match (hour, pm, am) {
            (12, true, _) => 12,
            (h, true, _) if h < 12 => h + 12,
            (12, _, true) => 0,
            (h, _, _) if h < 24 => h,
            _ => continue,
        };
        return Some((hour, minute));
    }
    None
}

/// The zone the person's Mac is in, which is the one they meant.
///
/// A schedule read out of "every morning at 7" is in the writer's own morning,
/// never in UTC, and a task that fires at 08:00 Vienna because nobody asked
/// which 7 was meant is a task nobody trusts again.
fn local_tz() -> String {
    let offset = chrono::Local::now().offset().to_string();
    // chrono gives the offset, not the name. The name is what a schedule needs
    // in order to survive a daylight-saving change, so it is read from the
    // system where that is possible and the offset kept as the honest
    // fallback.
    std::fs::read_link("/etc/localtime")
        .ok()
        .and_then(|p| {
            let p = p.to_string_lossy().to_string();
            p.split_once("zoneinfo/").map(|(_, z)| z.to_string())
        })
        .filter(|z| z.parse::<chrono_tz::Tz>().is_ok())
        .unwrap_or(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prohibition_beats_the_thing_it_forbids() {
        // The sentence that matters, written the way somebody really writes it.
        // Reading only its first half is how a task deletes somebody's post.
        let s = infer(
            "Look at the 5 most recent messages in my inbox. Tell me who each is from and its \
             subject. Do not open message bodies unless you must, and never move, file or delete \
             anything.",
        );
        assert_eq!(
            s.mail,
            Some(Mail { may_file: false }),
            "it must be allowed to read the mail and not to move it"
        );
        assert!(s.forbids.contains(&Forbid::MovingMail), "{:?}", s.forbids);
        assert!(
            s.forbids.contains(&Forbid::OpeningBodies),
            "{:?}",
            s.forbids
        );
    }

    #[test]
    fn message_bodies_are_not_a_refusal_to_message_people() {
        // "Do not open message bodies" is about reading. Matching "message" in
        // it told the person their task was held back from writing to anybody,
        // which they had not said and which was not true.
        let s = infer("Do not open message bodies unless you must.");
        assert!(
            !s.forbids.contains(&Forbid::Messaging),
            "reading was mistaken for writing: {:?}",
            s.forbids
        );
        assert!(s.forbids.contains(&Forbid::OpeningBodies));

        // And a real refusal to write is still caught.
        let s = infer("Summarise it, but never send anything to anyone.");
        assert!(s.forbids.contains(&Forbid::Messaging), "{:?}", s.forbids);
    }

    #[test]
    fn a_task_that_asks_for_filing_and_forbids_nothing_may_file() {
        let s = infer("Clean my mailbox of spam at 3am, moving junk to the Junk mailbox.");
        assert_eq!(s.mail, Some(Mail { may_file: true }));
        assert!(s.forbids.is_empty(), "{:?}", s.forbids);
    }

    #[test]
    fn clean_my_mailbox_but_never_delete_reads_as_one_sentence() {
        // Both halves are about the same mailbox and the second one wins.
        let s = infer("Clean up my mailbox, but never delete anything.");
        assert_eq!(
            s.mail,
            Some(Mail { may_file: false }),
            "a refusal to delete has to hold even next to a request to tidy"
        );
    }

    #[test]
    fn a_task_that_never_mentions_mail_gets_no_reach_into_it() {
        let s = infer("Open the Chrome browser and show me the CDV Software website.");
        assert_eq!(s.mail, None);
    }

    #[test]
    fn only_sites_that_are_actually_written_down_are_offered() {
        let s = infer("Book a court at https://tennis.example.com/booking, e.g. before 9am.");
        assert_eq!(s.domains, vec!["tennis.example.com".to_string()]);

        // The failure that started this rule: a name is not a domain, and a
        // guess that lands on a stranger's server may carry a login.
        assert!(infer("Show me the website of CDV Software")
            .domains
            .is_empty());
    }

    #[test]
    fn when_it_runs_is_read_from_the_way_people_write_it() {
        let cases = [
            ("Show me Yahoo every morning at 7am", "0 0 7 * * *"),
            ("Clean my mailbox from spam at 3am", "0 0 3 * * *"),
            ("Book a tennis court every Wednesday", "0 0 9 * * 3"),
            ("Show me important emails at 9am", "0 0 9 * * *"),
            ("Check the feed every hour", "0 0 * * * *"),
            ("Send me a digest every evening", "0 0 18 * * *"),
            ("Look at it every day at 7:30pm", "0 30 19 * * *"),
        ];
        for (text, expect) in cases {
            let s = infer(text);
            let got = s
                .schedule
                .as_ref()
                .and_then(|v| v.get("expr").and_then(|e| e.as_str()).map(str::to_string));
            assert_eq!(got.as_deref(), Some(expect), "for {text:?}");
        }
    }

    #[test]
    fn a_number_that_is_not_a_clock_does_not_become_one() {
        // "the 5 most recent messages" is the description that would otherwise
        // put a task on a schedule nobody asked for.
        let s = infer("Look at the 5 most recent messages in my inbox and tell me about them.");
        assert_eq!(s.schedule, None, "{:?}", s.schedule);
    }

    #[test]
    fn every_decision_says_which_words_caused_it() {
        // The report is the whole point: a setting nobody chose has to be able
        // to explain itself, or it is just a surprise.
        let s = infer("Clean my mailbox from spam at 3am");
        assert!(!s.notes.is_empty());
        for n in &s.notes {
            assert!(!n.what.trim().is_empty(), "{n:?}");
            assert!(!n.because.trim().is_empty(), "{n:?}");
        }
    }
}

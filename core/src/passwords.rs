//! Catching a password typed where instructions go.
//!
//! A task's description is the one field a person writes freely, so it is where
//! everything the task needs ends up, an account and its password included.
//! That is a reasonable thing to assume and the worst place in Errand to put
//! one. The description is not a note to self: it is the prompt, sent whole to
//! whichever model carries the task out, on every single run. It is stored in
//! the clear in the database, it appears on the screen, and the redactor cannot
//! help, because the redactor only masks values it was handed through the
//! keychain. A password typed here has left the machine before anybody notices
//! it was the wrong box.
//!
//! Errand already has the right box. A login saved in settings goes to the
//! macOS keychain, is bound to one site, and reaches a page only through
//! `fill_credential`, which types it without the model ever seeing it. So this
//! is not a rule against passwords. It is a signpost to the box next door,
//! raised at the moment somebody is typing, which is the only moment they are
//! in a position to do anything about it.
//!
//! Found by shape, not by cleverness: a word that announces a secret, a colon
//! or an equals or an "is", and something after it that looks like a value
//! rather than a sentence. That deliberately misses a password written with no
//! label at all, which cannot be told from any other word. Catching the shape
//! people actually type is worth more than catching every shape imaginable.

/// The words people put in front of a secret. Lowercase, matched whole.
///
/// German sits beside English because this is a Vienna-built app whose author
/// writes both, and "Passwort:" is not an exotic case here.
const LABELS: &[&str] = &[
    "password",
    "passwort",
    "passwd",
    "pwd",
    "pw",
    "kennwort",
    "passphrase",
    "api key",
    "api-key",
    "apikey",
    "access token",
    "token",
    "secret",
    "passcode",
    "pin",
    "otp",
];

/// What follows the label when somebody is pointing at a password rather than
/// typing one.
///
/// "Password: see the note in my keychain" is a person explaining where it
/// lives, which is exactly what this check wants them to do. Refusing that
/// would teach them the check is noise and to write around it.
const POINTING_NOT_TYPING: &[&str] = &[
    "see",
    "in",
    "the",
    "a",
    "my",
    "your",
    "our",
    "saved",
    "stored",
    "from",
    "same",
    "as",
    "unchanged",
    "none",
    "n/a",
    "na",
    "ask",
    "use",
    "using",
    "via",
    "whatever",
    "unknown",
    "tbd",
    "todo",
    "keychain",
    "settings",
    "above",
    "below",
    "it",
    "its",
    "it's",
    "there",
    "already",
    "under",
    "and",
    "or",
    "is",
    "was",
    "will",
    "should",
    "please",
    "not",
    "no",
    "empty",
    "blank",
];

/// A PIN is four digits and a password is longer. Anything shorter than this,
/// with letters in it, is a word rather than a secret.
const SHORTEST_PASSWORD: usize = 6;
const SHORTEST_PIN: usize = 4;

/// The label of the secret this text appears to contain, if it contains one.
///
/// Returns what was found rather than a bare yes, because the refusal names it:
/// somebody who wrote three lines needs to know which one to take out.
pub fn typed_secret(text: &str) -> Option<String> {
    text.lines().find_map(secret_on_line)
}

fn secret_on_line(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    for label in LABELS {
        let mut from = 0;
        while let Some(at) = lower[from..].find(label) {
            let start = from + at;
            let end = start + label.len();
            from = end;
            // "password" inside "passwordless" is not a label, and neither is
            // the "pin" in "shopping". A label is a word, with a boundary on
            // each side of it.
            if !boundary(lower.as_bytes(), start.checked_sub(1))
                || !boundary(lower.as_bytes(), Some(end))
            {
                continue;
            }
            if let Some(value) = value_after(&line[end..]) {
                if is_a_secret(label, &value) {
                    return Some((*label).to_string());
                }
            }
        }
    }
    None
}

/// Is this position outside the word, rather than more of it?
fn boundary(bytes: &[u8], at: Option<usize>) -> bool {
    match at {
        None => true,
        Some(i) => match bytes.get(i) {
            None => true,
            Some(c) => !c.is_ascii_alphanumeric(),
        },
    }
}

/// What comes after the label, once the separator is out of the way.
///
/// A separator is required. "The password box is on the right" names no value,
/// and treating the next word as one would refuse half the descriptions people
/// write about logging in to anything.
fn value_after(rest: &str) -> Option<String> {
    let rest = rest.trim_start();
    let rest = if let Some(r) = rest.strip_prefix(':') {
        r
    } else if let Some(r) = rest.strip_prefix('=') {
        r
    } else if let Some(r) = rest.strip_prefix("is ") {
        r
    } else if let Some(r) = rest.strip_prefix("ist ") {
        r
    } else {
        return None;
    };
    // The first thing after it, whatever else the line goes on to say. A value
    // with a comment after it is still a value.
    rest.split_whitespace().next().map(str::to_string)
}

fn is_a_secret(label: &str, value: &str) -> bool {
    // Punctuation belongs to the sentence, not to the secret: "password: x."
    // ends a line and "(password: x)" is somebody's aside.
    let value = value.trim_matches(|c: char| matches!(c, '.' | ',' | ';' | ')' | '"' | '\'' | '»'));
    if value.is_empty() {
        return false;
    }
    if POINTING_NOT_TYPING.contains(&value.to_ascii_lowercase().as_str()) {
        return false;
    }
    let digits_only = value.chars().all(|c| c.is_ascii_digit());
    let shortest = if digits_only && matches!(label, "pin" | "otp" | "passcode") {
        SHORTEST_PIN
    } else {
        SHORTEST_PASSWORD
    };
    value.chars().count() >= shortest
}

/// What to say to somebody who has just typed one, and where to put it instead.
///
/// Names the box and the two properties that make it the right box, because
/// "use the credential store" means nothing to somebody who has not found it
/// yet, and a refusal that does not say where to go is a wall.
pub fn refusal(label: &str) -> String {
    format!(
        "That looks like a {label} typed into the description, so nothing was saved. The \
         description is sent whole to whichever model carries this task out, on every run, and it \
         is kept in the clear, so a secret written here has left this Mac before anybody notices \
         it was the wrong box. Take that line out and put the login in Settings under Logins \
         instead: it goes into your macOS keychain, it is tied to the one site you name, and the \
         task can then use it without the model, the screen or the log ever seeing it. If you \
         only meant to say where the password lives, write that in words and this will accept it."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_typed_into_a_description_is_caught_and_named() {
        // The real one. A task set up to read somebody's X feed carried the
        // account and its password in the description, which meant both went
        // to the model on all three of its runs before anybody noticed.
        let found = typed_secret(
            "Show me the trending subjects on X\n\nX user: someone@example.com\nX PW: Hunter2-not-a-real-one",
        );
        assert_eq!(found.as_deref(), Some("pw"));
        let said = refusal(&found.unwrap());
        assert!(said.contains("Logins"), "{said}");
        assert!(said.contains("keychain"), "{said}");
        assert!(said.contains("nothing was saved"), "{said}");
    }

    #[test]
    fn every_way_a_person_writes_it_down_is_caught() {
        for line in [
            "password: hunter22",
            "Password = hunter22",
            "my password is hunter22",
            "Passwort: geheim123",
            "api key: sk-abc123def",
            "API-KEY=sk-abc123def",
            "token: ghp_aaaabbbbcccc",
            "PIN: 4831",
            "otp: 583921",
        ] {
            assert!(
                typed_secret(line).is_some(),
                "went through unchallenged: {line}"
            );
        }
    }

    #[test]
    fn talking_about_a_password_is_not_typing_one() {
        // These are the sentences people actually write about logging in. A
        // check that refuses them is a check that teaches everybody to write
        // around it, and then it catches nothing at all.
        for line in [
            "Log in with the saved password.",
            "The password box is on the right, under the username.",
            "Password: see the login saved in Errand",
            "password: stored in my keychain",
            "If it asks for a password, use the one saved for this site.",
            "Sign in with the saved X login when the site asks for one.",
            "Reset the password if it has expired and tell me.",
            "The site is passwordless: it sends a code to my phone.",
            "Check the pinned post first.",
            "Token expiry is a known problem on this site.",
        ] {
            assert_eq!(typed_secret(line), None, "refused ordinary prose: {line}");
        }
    }

    #[test]
    fn a_short_word_after_the_label_is_not_mistaken_for_a_secret() {
        assert_eq!(typed_secret("password: none"), None);
        assert_eq!(typed_secret("password: n/a"), None);
        // But four digits after "PIN" is a PIN, short as it is.
        assert_eq!(typed_secret("PIN: 1234").as_deref(), Some("pin"));
        // and four characters after "password" is a word, not a password.
        assert_eq!(typed_secret("password: card"), None);
    }

    #[test]
    fn a_secret_is_found_wherever_in_the_description_it_sits() {
        let text = "Book the Wednesday court.\nThe club site is slow, be patient.\n\
                    Login: me@example.com pwd=Tr0ub4dor3\nTell me if it worked.";
        assert_eq!(typed_secret(text).as_deref(), Some("pwd"));
    }

    #[test]
    fn a_label_inside_a_longer_word_is_not_a_label() {
        // "spin:" and "weapon:" both end in a label if you do not look for the
        // edges of the word.
        assert_eq!(typed_secret("spin: 123456"), None);
        assert_eq!(
            typed_secret("The endpoint token_bucket=500000 is the limit"),
            None
        );
    }
}

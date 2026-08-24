//! Keeping secrets out of everything that leaves the run.
//!
//! Every byte headed for the journal, the logs, an AI prompt, a notification,
//! or an API response passes through here first. The registry is seeded with
//! each secret actually resolved during the run, plus the encodings a value
//! picks up on the way through a browser (URL-escaped, base64), plus a few
//! shapes that are always secrets no matter where they came from.
//!
//! Note what this is not: it removes known secret values, not every personal
//! detail on a page. A page you are logged into still contains your name and
//! your bookings, and if the run uses a cloud model, that content is part of
//! what the provider sees. Say so plainly in the privacy docs rather than
//! implying redaction makes cloud runs private.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

/// Shapes that are secrets wherever they appear.
const ALWAYS: &[(&str, &str)] = &[
    ("Bearer ", "[SECRET:bearer]"),
    ("Basic ", "[SECRET:basic-auth]"),
];

#[derive(Clone, Default)]
pub struct Redactor {
    inner: Arc<RwLock<Inner>>,
}

#[derive(Default)]
struct Inner {
    /// value -> label, longest first when scrubbing so a secret that contains
    /// another secret is replaced as a whole.
    entries: HashMap<String, String>,
}

impl Redactor {
    /// Register a secret and every encoding of it worth catching.
    ///
    /// Short values are ignored: registering something like "a" or "12" would
    /// turn every journal entry into confetti, and a secret that short is not
    /// protectable by string matching anyway.
    pub fn register(&self, value: &str, label: &str) {
        if value.len() < 4 {
            return;
        }
        let mut g = self.inner.write();
        let mark = format!("[SECRET:{label}]");
        g.entries.insert(value.to_string(), mark.clone());

        let url = urlencode(value);
        if url != value {
            g.entries.insert(url, mark.clone());
        }
        let b64 = b64(value.as_bytes());
        g.entries.insert(b64, mark.clone());

        // A TOTP code is only live for a step or two, but it is a credential
        // while it lives, so it is registered like any other.
        let json_escaped = value.replace('"', "\\\"");
        if json_escaped != value {
            g.entries.insert(json_escaped, mark);
        }
    }

    /// Replace every known secret in `text`.
    pub fn scrub(&self, text: &str) -> String {
        let g = self.inner.read();
        if g.entries.is_empty() {
            return scrub_always(text);
        }
        // Longest first: otherwise a shorter secret that is a substring of a
        // longer one leaves the tail of the longer one exposed.
        let mut keys: Vec<&String> = g.entries.keys().collect();
        keys.sort_by_key(|k| std::cmp::Reverse(k.len()));

        let mut out = text.to_string();
        for k in keys {
            if out.contains(k.as_str()) {
                out = out.replace(k.as_str(), &g.entries[k]);
            }
        }
        scrub_always(&out)
    }

    /// True when the text still contains a registered secret. Used by tests and
    /// by the assertion on anything about to be written.
    pub fn is_clean(&self, text: &str) -> bool {
        let g = self.inner.read();
        !g.entries.keys().any(|k| text.contains(k.as_str()))
    }
}

fn scrub_always(text: &str) -> String {
    let mut out = text.to_string();
    for (prefix, mark) in ALWAYS {
        // Replace the token that follows the prefix, not the prefix itself.
        while let Some(i) = out.find(prefix) {
            let start = i + prefix.len();
            let end = out[start..]
                .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
                .map(|o| start + o)
                .unwrap_or(out.len());
            if end <= start {
                break;
            }
            out.replace_range(i..end, mark);
        }
    }
    out
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn b64(input: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_a_secret_with_its_label() {
        let r = Redactor::default();
        r.register("hunter2horse", "Club password");
        let out = r.scrub("typed hunter2horse into the box");
        assert_eq!(out, "typed [SECRET:Club password] into the box");
        assert!(!out.contains("hunter2horse"));
    }

    #[test]
    fn catches_url_and_base64_encodings() {
        let r = Redactor::default();
        r.register("p@ss word!", "cred");
        let url = urlencode("p@ss word!");
        let enc = b64(b"p@ss word!");
        assert!(!r.scrub(&format!("q={url}")).contains(&url));
        assert!(!r.scrub(&format!("auth {enc}")).contains(&enc));
    }

    #[test]
    fn longest_secret_wins_so_no_tail_survives() {
        let r = Redactor::default();
        r.register("abcd", "short");
        r.register("abcdefgh", "long");
        let out = r.scrub("value abcdefgh here");
        assert!(!out.contains("abcdefgh"));
        assert!(
            !out.contains("efgh"),
            "tail of the longer secret leaked: {out}"
        );
    }

    #[test]
    fn strips_bearer_tokens_it_was_never_told_about() {
        let r = Redactor::default();
        let out = r.scrub("Authorization: Bearer err_v1_deadbeefcafe");
        assert!(!out.contains("deadbeef"));
        assert!(out.contains("[SECRET:bearer]"));
    }

    #[test]
    fn ignores_values_too_short_to_match_safely() {
        let r = Redactor::default();
        r.register("ab", "tiny");
        assert_eq!(r.scrub("a fabulous cab"), "a fabulous cab");
    }

    #[test]
    fn is_clean_detects_an_unscrubbed_secret() {
        let r = Redactor::default();
        r.register("topsecretvalue", "x");
        assert!(!r.is_clean("contains topsecretvalue"));
        assert!(r.is_clean(&r.scrub("contains topsecretvalue")));
    }
}

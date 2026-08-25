//! Which sites a task may open, written down the way the check will read it.
//!
//! The run-time allowlist check lives in the runner: it takes the host out of
//! the URL it is about to open, lowercases it, and asks whether it equals a
//! stored entry or ends with a dot followed by one. Nothing else. There is no
//! wildcard support, no scheme handling, no port stripping, no fuzzy matching.
//!
//! That makes almost every plausible thing a person types silently useless.
//! `https://example.com/` stores happily and matches nothing, because the check
//! compares against a bare host. `*.example.com` is a legal URL host, so it
//! stores happily too, and also matches nothing. So does `Example.COM ` with a
//! stray space, or an address typed in a non-Latin script. A task saved with
//! any of those is a task that cannot open a single page, and it says so only
//! at the moment it runs.
//!
//! So everything goes through here first. Normalisation is built on the same
//! parser the run-time check uses, `url::Url`, which is what makes the stored
//! form byte-identical to what will be compared rather than merely similar:
//! case folding, punycode, percent-decoding and the rest all come from the one
//! code path. Where the intent is unambiguous the input is simply tidied. Where
//! it is not, it is refused with a sentence saying what to type instead, because
//! a refusal at the moment of typing costs a person ten seconds and a silent
//! non-match costs them the booking.

use anyhow::{bail, Result};

/// Two-label endings that belong to everybody rather than to somebody.
///
/// Not a full public suffix list, and not meant to be. These are the ones a
/// person actually types when they mean their own site, and each would hand the
/// task every site registered underneath it.
const PUBLIC_SUFFIXES: &[&str] = &[
    "co.uk", "com.au", "co.jp", "org.uk", "ac.uk", "com.br", "co.nz", "co.za",
];

/// A tidied list of sites, plus anything the person ought to hear about it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Normalized {
    /// What to store, in the order it was given.
    pub domains: Vec<String>,
    /// Plain sentences to show alongside the saved list. Not errors: the list
    /// is valid and has been accepted.
    pub warnings: Vec<String>,
}

/// Turn what a person typed into the exact string the run-time check compares.
///
/// Tidied silently: a scheme, a path, a query, a fragment, a trailing slash,
/// userinfo, a port, surrounding whitespace, capital letters, a root dot, and
/// an address in another script. Refused with an explanation: anything where
/// guessing the intent would mean guessing wrong.
pub fn normalize_domain(input: &str) -> Result<String> {
    let typed = input.trim();
    if typed.is_empty() {
        bail!(
            "One of the site entries is blank. Type the address of a site this task is allowed \
             to open, like example.com, or remove the empty line."
        );
    }

    // Parse the way the run-time check parses. Anything without a scheme gets
    // the one the check will most often see, purely so there is a URL to take a
    // host out of; the scheme itself is never stored.
    let as_url = if has_scheme(typed) {
        typed.to_string()
    } else {
        format!("https://{typed}/")
    };

    let Ok(parsed) = url::Url::parse(&as_url) else {
        bail!(
            "'{typed}' is not an address Errand can read, so it has not been saved. Type just \
             the site itself, like example.com, with no spaces in it."
        );
    };
    let Some(host) = parsed.host_str() else {
        bail!(
            "'{typed}' has no site name in it, so there would be nothing to match against. Type \
             the address itself, like example.com."
        );
    };

    // Hosts come back lowercased for the ordinary web schemes, but not for
    // every scheme, and the check compares lowercase. Fold it here so the two
    // can never disagree.
    let host = host.to_ascii_lowercase();

    // An IPv6 literal keeps its brackets: that is the form the check will see
    // coming back out of the parser, and both sides compare it exactly.
    if host.starts_with('[') {
        return Ok(host);
    }

    // The root dot is invisible to whoever typed it and fatal to the match, as
    // a live page's host never carries one.
    let host = host.trim_end_matches('.').to_string();

    if host.contains('*') {
        let apex = host
            .rsplit('*')
            .next()
            .unwrap_or("")
            .trim_start_matches('.');
        let advice = if apex.is_empty() {
            "Type the address on its own, like example.com.".to_string()
        } else {
            format!("Type {apex} instead.")
        };
        bail!(
            "'{typed}' has a * in it. Errand compares addresses exactly, so a site saved that \
             way would never open — not one page. Subdomains are already included anyway: \
             example.com covers www.example.com and everything else under it. {advice}"
        );
    }

    if let Some(bad) = host
        .chars()
        .find(|c| !matches!(c, 'a'..='z' | '0'..='9' | '.' | '-'))
    {
        bail!(
            "'{typed}' contains '{bad}', which is not part of a website address. Errand would \
             store it and then never match anything with it. Type just the address, like \
             example.com."
        );
    }

    // `url::Url` reads a bare number as an IPv4 address: "1" becomes 0.0.0.1
    // and "2130706433" becomes 127.0.0.1. Left alone, a slip of the keyboard
    // would be saved as a perfectly valid-looking entry pointing at a machine
    // nobody meant to allow.
    if !typed.contains('.') && host.contains('.') {
        bail!(
            "'{typed}' is read as the machine address {host}, which is almost certainly not the \
             site you meant. Nothing has been saved. Type the site's name, like example.com."
        );
    }

    // A task against something running on this Mac is a fair thing to want, and
    // both sides compare it by exact equality.
    if host == "localhost" {
        return Ok(host);
    }

    let labels: Vec<&str> = host.split('.').collect();
    if labels.iter().any(|l| l.is_empty()) {
        bail!(
            "'{typed}' has an empty piece in it, so it would never match a real address. Type it \
             the way it appears in the browser's address bar, like example.com."
        );
    }
    if labels.len() < 2 {
        bail!(
            "'{host}' on its own would let this task open every site whose address ends in \
             .{host}, which is far more than it needs. Type the whole address, like example.com."
        );
    }
    if PUBLIC_SUFFIXES.contains(&host.as_str()) {
        bail!(
            "'{host}' is the ending shared by every site registered under it, so allowing it \
             would allow all of them. Type the whole address, like example.{host}."
        );
    }

    Ok(host)
}

/// Tidy a whole list, keeping the order it was given in.
///
/// Order is load-bearing, which is easy to miss. The runner picks the browser
/// profile for a run — the profile holding whatever this task is already signed
/// in to — from the FIRST entry in this list. Reordering it therefore changes
/// which profile the task uses, and a task that was logged in yesterday can
/// find itself logged out today with nothing else having changed. So repeats
/// are dropped at their later position and the first mention keeps its place.
///
/// Returns the warnings alongside, so the caller can show them next to the
/// saved list. They are not refusals: the list has been accepted.
pub fn normalize_domains(inputs: &[String]) -> Result<Normalized> {
    let mut domains: Vec<String> = Vec::with_capacity(inputs.len());
    for raw in inputs {
        let d = normalize_domain(raw)?;
        if !domains.contains(&d) {
            domains.push(d);
        }
    }
    let warnings = missing_apex_warnings(&domains);
    Ok(Normalized { domains, warnings })
}

/// Warn about a list that allows `www.x.com` but not `x.com`.
///
/// Matching runs one way only: `example.com` permits `www.example.com`, but
/// `www.example.com` does not permit `example.com`. Most sites bounce between
/// the two on the first request, so a list holding only the www form tends to
/// fail on the very first navigation, before the task has done anything at all.
/// Worth saying out loud, but not worth refusing: allowing only the www form is
/// a legitimate thing to want.
pub fn missing_apex_warnings(domains: &[String]) -> Vec<String> {
    let mut out = vec![];
    for d in domains {
        let Some(apex) = d.strip_prefix("www.") else {
            continue;
        };
        if domains.iter().any(|other| other == apex) {
            continue;
        }
        // Only suggest an apex that would itself be accepted, so this never
        // talks somebody into typing "co.uk".
        if normalize_domain(apex).map(|a| a == apex).unwrap_or(false) {
            out.push(format!(
                "This task is allowed to open {d} but not {apex}. Most sites move between the \
                 two, so the very first page it opens may be blocked. Adding {apex} as well \
                 would avoid that."
            ));
        }
    }
    out
}

/// Does this look like `scheme://…` rather than a bare address?
///
/// Checked by hand rather than by trying the parser, because `example.com:8443`
/// parses as a scheme called `example.com` and would lose the host entirely.
fn has_scheme(s: &str) -> bool {
    match s.find("://") {
        Some(i) if i > 0 => {
            let scheme = &s[..i];
            scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The run-time rule, copied from runner/src/browser.rs, so a stored entry
    /// can be tested against what will actually compare it. If the two ever
    /// drift, the round-trip test below is what notices.
    fn permits(entry: &str, url: &str) -> bool {
        let host = url::Url::parse(url)
            .unwrap()
            .host_str()
            .unwrap()
            .to_ascii_lowercase();
        let d = entry.trim().to_ascii_lowercase();
        !d.is_empty() && (host == d || host.ends_with(&format!(".{d}")))
    }

    #[test]
    fn a_pasted_url_is_reduced_to_the_bare_host_that_will_be_compared() {
        assert_eq!(
            normalize_domain("https://www.example.com/basket?id=7#top").unwrap(),
            "www.example.com"
        );
        assert_eq!(
            normalize_domain("http://user:pw@example.com:8443/a/b").unwrap(),
            "example.com"
        );
    }

    #[test]
    fn capitals_spaces_and_a_trailing_slash_are_tidied_rather_than_refused() {
        assert_eq!(normalize_domain("  EXAMPLE.com  ").unwrap(), "example.com");
        assert_eq!(normalize_domain("Example.Com/").unwrap(), "example.com");
        assert_eq!(normalize_domain("example.com.").unwrap(), "example.com");
    }

    #[test]
    fn a_wildcard_is_refused_because_it_would_quietly_match_nothing_at_all() {
        let e = normalize_domain("*.example.com").unwrap_err().to_string();
        assert!(e.contains("example.com"), "no advice in: {e}");
        assert!(
            e.contains("Subdomains are already included"),
            "does not explain why it is unnecessary: {e}"
        );
    }

    #[test]
    fn a_single_label_is_refused_because_it_would_allow_a_whole_top_level_domain() {
        let e = normalize_domain("com").unwrap_err().to_string();
        assert!(e.contains("every site"), "does not say how wide it is: {e}");
    }

    #[test]
    fn a_bare_public_suffix_is_refused_because_it_would_allow_everyone_under_it() {
        for s in ["co.uk", "com.au", "co.jp", "ac.uk"] {
            let e = normalize_domain(s).unwrap_err().to_string();
            assert!(e.contains("every site"), "{s} was not explained: {e}");
        }
        // The same ending with a real name in front is an ordinary site.
        assert_eq!(
            normalize_domain("tennis-club.co.uk").unwrap(),
            "tennis-club.co.uk"
        );
    }

    #[test]
    fn an_address_in_another_script_is_stored_as_the_browser_will_report_it() {
        let stored = normalize_domain("bücher.example").unwrap();
        assert_eq!(stored, "xn--bcher-kva.example");
        assert!(
            permits(&stored, "https://bücher.example/shop"),
            "the punycode form must match the address as typed"
        );
    }

    #[test]
    fn a_comma_is_refused_because_it_would_break_the_playbook_round_trip() {
        // A comma is a legal URL host character, and the playbook writes the
        // site list out joined with ", " and reads it back split on ",". One
        // comma in an entry and the list comes back as something else.
        let e = normalize_domain("a,b.example.com").unwrap_err().to_string();
        assert!(e.contains("','"), "does not name the character: {e}");
    }

    #[test]
    fn a_bare_number_is_refused_rather_than_becoming_an_ip_address_nobody_typed() {
        for typo in ["1", "2130706433", "0x7f000001"] {
            let e = normalize_domain(typo).unwrap_err().to_string();
            assert!(
                e.contains("machine address"),
                "{typo} was accepted or unexplained: {e}"
            );
        }
    }

    #[test]
    fn a_task_against_something_on_this_machine_is_still_allowed() {
        assert_eq!(normalize_domain("localhost").unwrap(), "localhost");
        assert_eq!(normalize_domain("127.0.0.1").unwrap(), "127.0.0.1");
        assert_eq!(normalize_domain("[::1]").unwrap(), "[::1]");
        assert_eq!(
            normalize_domain("http://localhost:3000/health").unwrap(),
            "localhost"
        );
    }

    #[test]
    fn an_empty_entry_is_refused_with_something_to_do_about_it() {
        for blank in ["", "   ", "\t\n"] {
            let e = normalize_domain(blank).unwrap_err().to_string();
            assert!(e.contains("example.com"), "no advice given: {e}");
        }
    }

    #[test]
    fn what_is_stored_is_exactly_what_the_run_time_check_compares() {
        let stored = normalize_domain("HTTPS://Example.COM/basket/").unwrap();
        assert_eq!(stored, "example.com");

        assert!(permits(&stored, "https://example.com/"));
        assert!(permits(&stored, "https://www.example.com/basket"));
        assert!(permits(&stored, "https://shop.eu.example.com/"));
        assert!(!permits(&stored, "https://notexample.com/"));
        assert!(!permits(&stored, "https://example.com.evil.test/"));
    }

    #[test]
    fn the_www_form_alone_does_not_let_the_task_reach_the_plain_address() {
        // One-directional, and the reason the warning below exists.
        let stored = normalize_domain("www.example.com").unwrap();
        assert!(permits(&stored, "https://www.example.com/"));
        assert!(!permits(&stored, "https://example.com/"));
    }

    #[test]
    fn the_order_of_the_list_survives_and_repeats_are_dropped() {
        let input: Vec<String> = ["tennis-club.example", "https://EXAMPLE.com/", "example.com"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out = normalize_domains(&input).unwrap();
        assert_eq!(out.domains, vec!["tennis-club.example", "example.com"]);
        assert_eq!(
            out.domains[0], "tennis-club.example",
            "the first entry picks the browser profile, so it must not move"
        );
    }

    #[test]
    fn a_www_entry_without_its_plain_address_is_warned_about_but_still_saved() {
        let input = vec!["www.example.com".to_string()];
        let out = normalize_domains(&input).unwrap();
        assert_eq!(out.domains, vec!["www.example.com"]);
        assert_eq!(out.warnings.len(), 1);
        assert!(
            out.warnings[0].contains("Adding example.com"),
            "{:?}",
            out.warnings
        );

        let both = vec!["www.example.com".to_string(), "example.com".to_string()];
        assert!(normalize_domains(&both).unwrap().warnings.is_empty());
    }

    #[test]
    fn the_warning_never_suggests_adding_a_public_suffix() {
        let input = vec!["www.co.uk".to_string()];
        let out = normalize_domains(&input).unwrap();
        assert!(
            out.warnings.is_empty(),
            "it must not talk anyone into allowing co.uk: {:?}",
            out.warnings
        );
    }

    #[test]
    fn one_bad_entry_refuses_the_whole_list_rather_than_saving_part_of_it() {
        let input = vec!["example.com".to_string(), "*.example.org".to_string()];
        assert!(normalize_domains(&input).is_err());
    }
}

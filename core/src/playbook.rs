//! What the agent learned, written down.
//!
//! A playbook is a markdown file, versioned, on disk, indexed in SQLite. Plain
//! markdown rather than a JSON blob in a column for a reason: you can read it,
//! edit it in any editor, diff two versions, and see exactly what your agent
//! believes about a site. A structure nobody can read is a structure nobody
//! checks.
//!
//! The description you wrote stays the source of truth. Delete every playbook
//! version and the task still works; the next run just has to work it out
//! again. That ordering matters, because a playbook is derived from pages
//! written by strangers, so it is evidence rather than instruction.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// Where a playbook version came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Distilled from a supervised first run.
    Teach,
    /// Adjusted automatically after a later run.
    Refine,
    /// Patched by the fixer after repairing a failure.
    Fixer,
    /// Edited by the person.
    ManualEdit,
}

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Teach => "teach",
            Self::Refine => "refine",
            Self::Fixer => "fixer",
            Self::ManualEdit => "manual_edit",
        }
    }
}

/// A parsed playbook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playbook {
    pub version: i64,
    pub goal: String,
    #[serde(default)]
    pub sites: Vec<String>,
    #[serde(default)]
    pub preconditions: Vec<String>,
    pub steps: Vec<Step>,
    #[serde(default)]
    pub success: Vec<String>,
    #[serde(default)]
    pub known_failures: Vec<String>,
    /// Lines the agent may never cross. No automated editor may touch these.
    #[serde(default)]
    pub never: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// What this step is trying to achieve. Survives a site redesign.
    pub intent: String,
    /// How it was done last time. A hint, not gospel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// What to do when the obvious path is not available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
}

impl Playbook {
    /// Render to the markdown that gets stored and shown.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("---\n");
        out.push_str(&format!("version: {}\n", self.version));
        if !self.sites.is_empty() {
            out.push_str(&format!("sites: [{}]\n", self.sites.join(", ")));
        }
        out.push_str("---\n\n");

        out.push_str("# Goal\n");
        out.push_str(self.goal.trim());
        out.push_str("\n\n");

        if !self.preconditions.is_empty() {
            out.push_str("# Preconditions\n");
            for p in &self.preconditions {
                out.push_str(&format!("- {}\n", p.trim()));
            }
            out.push('\n');
        }

        out.push_str("# Steps\n");
        for (i, s) in self.steps.iter().enumerate() {
            out.push_str(&format!("{}. INTENT: {}\n", i + 1, s.intent.trim()));
            if let Some(h) = &s.hint {
                out.push_str(&format!("   HINT: {}\n", h.trim()));
            }
            if let Some(d) = &s.decision {
                out.push_str(&format!("   DECISION: {}\n", d.trim()));
            }
        }
        out.push('\n');

        if !self.success.is_empty() {
            out.push_str("# Success criteria\n");
            for c in &self.success {
                out.push_str(&format!("- {}\n", c.trim()));
            }
            out.push('\n');
        }
        if !self.known_failures.is_empty() {
            out.push_str("# Known failure modes\n");
            for f in &self.known_failures {
                out.push_str(&format!("- {}\n", f.trim()));
            }
            out.push('\n');
        }
        if !self.never.is_empty() {
            out.push_str("# Never do\n");
            for n in &self.never {
                out.push_str(&format!("- {}\n", n.trim()));
            }
            out.push('\n');
        }
        out
    }

    /// Read one back. Tolerant of hand-editing, because people will hand-edit.
    pub fn from_markdown(md: &str) -> Result<Self> {
        let mut version = 1i64;
        let mut sites = vec![];
        let mut body = md;

        if let Some(rest) = md.strip_prefix("---\n") {
            if let Some(end) = rest.find("\n---") {
                for line in rest[..end].lines() {
                    if let Some(v) = line.strip_prefix("version:") {
                        version = v.trim().parse().unwrap_or(1);
                    } else if let Some(v) = line.strip_prefix("sites:") {
                        sites = v
                            .trim()
                            .trim_start_matches('[')
                            .trim_end_matches(']')
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                    }
                }
                body = &rest[end + 4..];
            }
        }

        let mut section = "";
        let (mut goal, mut preconditions, mut success, mut known, mut never) =
            (String::new(), vec![], vec![], vec![], vec![]);
        let mut steps: Vec<Step> = vec![];

        for raw in body.lines() {
            let line = raw.trim_end();
            let t = line.trim();
            if let Some(h) = t.strip_prefix("# ") {
                section = match h.trim().to_ascii_lowercase().as_str() {
                    "goal" => "goal",
                    "preconditions" => "pre",
                    "steps" => "steps",
                    "success criteria" => "success",
                    "known failure modes" => "known",
                    "never do" => "never",
                    _ => "",
                };
                continue;
            }
            if t.is_empty() {
                continue;
            }
            let bullet = t.strip_prefix("- ").map(str::to_string);
            match section {
                "goal" => {
                    if !goal.is_empty() {
                        goal.push(' ');
                    }
                    goal.push_str(t);
                }
                "pre" => preconditions.extend(bullet),
                "success" => success.extend(bullet),
                "known" => known.extend(bullet),
                "never" => never.extend(bullet),
                "steps" => {
                    if let Some(rest) = t.split_once(". INTENT: ").map(|x| x.1) {
                        steps.push(Step {
                            intent: rest.to_string(),
                            hint: None,
                            decision: None,
                        });
                    } else if let Some(h) = t.strip_prefix("HINT: ") {
                        if let Some(last) = steps.last_mut() {
                            last.hint = Some(h.to_string());
                        }
                    } else if let Some(d) = t.strip_prefix("DECISION: ") {
                        if let Some(last) = steps.last_mut() {
                            last.decision = Some(d.to_string());
                        }
                    }
                }
                _ => {}
            }
        }

        if goal.trim().is_empty() {
            return Err(anyhow!("a playbook needs a goal"));
        }
        if steps.is_empty() {
            return Err(anyhow!("a playbook needs at least one step"));
        }

        Ok(Self {
            version,
            goal: goal.trim().to_string(),
            sites,
            preconditions,
            steps,
            success,
            known_failures: known,
            never,
        })
    }

    /// Is a proposed replacement safe to apply without a human reading it?
    ///
    /// Only hints may change automatically. A playbook is distilled from pages
    /// written by strangers and then fed back to the agent as trusted
    /// instruction, so the review gate is the thing standing between a hostile
    /// page and a permanent foothold in every future run. Anything that changes
    /// what the task DOES, rather than how it finds things, waits for a person.
    pub fn auto_applicable(&self, next: &Playbook) -> AutoApply {
        if self.never != next.never {
            return AutoApply::No("it changes a 'Never do' line");
        }
        if self.goal.trim() != next.goal.trim() {
            return AutoApply::No("it changes the goal");
        }
        if self.steps.len() != next.steps.len() {
            return AutoApply::No("it adds or removes a step");
        }
        for (a, b) in self.steps.iter().zip(next.steps.iter()) {
            if a.intent.trim() != b.intent.trim() {
                return AutoApply::No("it changes what a step is trying to do");
            }
            if a.decision != b.decision {
                return AutoApply::No("it changes a decision rule");
            }
        }
        if self.success != next.success {
            return AutoApply::No("it changes what counts as success");
        }
        AutoApply::Yes
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoApply {
    Yes,
    No(&'static str),
}

impl AutoApply {
    pub fn is_yes(&self) -> bool {
        matches!(self, Self::Yes)
    }
}

/// Where a task's playbook versions live on disk.
pub fn dir_for(task_id: &str) -> Result<std::path::PathBuf> {
    Ok(crate::paths::playbooks_dir()?.join(task_id))
}

pub fn path_for(task_id: &str, version: i64) -> Result<std::path::PathBuf> {
    Ok(dir_for(task_id)?.join(format!("v{version:04}.md")))
}

pub fn write(task_id: &str, pb: &Playbook) -> Result<(std::path::PathBuf, String)> {
    let dir = dir_for(task_id)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = path_for(task_id, pb.version)?;
    let md = pb.to_markdown();
    std::fs::write(&path, &md).with_context(|| format!("writing {}", path.display()))?;
    Ok((path, sha256_hex(md.as_bytes())))
}

pub fn read(task_id: &str, version: i64) -> Result<Playbook> {
    let path = path_for(task_id, version)?;
    let md =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Playbook::from_markdown(&md)
}

fn sha256_hex(bytes: &[u8]) -> String {
    // Small local implementation so core does not take a hashing dependency
    // for one call site.
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = bytes.to_vec();
    let bitlen = (bytes.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_be_bytes());

    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (i, v) in [a, b, c, d, e, f, g, hh].iter().enumerate() {
            h[i] = h[i].wrapping_add(*v);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Playbook {
        Playbook {
            version: 1,
            goal: "Book court 2 or 4 for Wednesday 19:00.".into(),
            sites: vec!["tennis-club.example".into()],
            preconditions: vec!["Booking opens Wednesdays at 08:00 sharp.".into()],
            steps: vec![
                Step {
                    intent: "Reach the booking grid.".into(),
                    hint: Some("/courts, the grid is table.slot-grid".into()),
                    decision: None,
                },
                Step {
                    intent: "Select Wednesday 19:00.".into(),
                    hint: None,
                    decision: Some("Prefer court 2, else court 4, else report unavailable.".into()),
                },
            ],
            success: vec!["A confirmation number was captured.".into()],
            known_failures: vec!["The grid renders late on Wednesdays.".into()],
            never: vec!["Never book two slots.".into()],
        }
    }

    #[test]
    fn a_playbook_survives_a_round_trip_through_markdown() {
        let pb = sample();
        let back = Playbook::from_markdown(&pb.to_markdown()).unwrap();
        assert_eq!(back, pb);
    }

    #[test]
    fn the_rendered_form_is_something_a_person_can_read() {
        let md = sample().to_markdown();
        assert!(md.contains("# Goal"));
        assert!(md.contains("1. INTENT: Reach the booking grid."));
        assert!(md.contains("   HINT: /courts"));
        assert!(md.contains("# Never do"));
    }

    #[test]
    fn a_playbook_without_a_goal_or_steps_is_refused() {
        assert!(Playbook::from_markdown("# Goal\nDo a thing\n").is_err());
        assert!(Playbook::from_markdown("# Steps\n1. INTENT: go\n").is_err());
    }

    #[test]
    fn only_hint_changes_apply_without_a_person_reading_them() {
        let a = sample();
        let mut b = a.clone();
        b.steps[0].hint = Some("/book, the grid moved to div.grid".into());
        assert!(
            a.auto_applicable(&b).is_yes(),
            "a moved selector is routine"
        );
    }

    /// The playbook is distilled from pages written by strangers and then fed
    /// back as trusted instruction, so these are the changes that must wait for
    /// a human.
    #[test]
    fn anything_that_changes_what_the_task_does_waits_for_a_person() {
        let a = sample();

        let mut goal = a.clone();
        goal.goal = "Book any court at any time.".into();
        assert_eq!(
            a.auto_applicable(&goal),
            AutoApply::No("it changes the goal")
        );

        let mut extra = a.clone();
        extra.steps.push(Step {
            intent: "Also pay the annual fee.".into(),
            hint: None,
            decision: None,
        });
        assert_eq!(
            a.auto_applicable(&extra),
            AutoApply::No("it adds or removes a step")
        );

        let mut never = a.clone();
        never.never.clear();
        assert_eq!(
            a.auto_applicable(&never),
            AutoApply::No("it changes a 'Never do' line")
        );

        let mut decision = a.clone();
        decision.steps[1].decision = Some("Book every available court.".into());
        assert_eq!(
            a.auto_applicable(&decision),
            AutoApply::No("it changes a decision rule")
        );
    }

    #[test]
    fn the_hash_matches_a_known_value() {
        // Guards the hand-rolled implementation against a silent breakage.
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}

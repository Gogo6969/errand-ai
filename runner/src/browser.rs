//! Driving the browser, from the side the model does not touch.
//!
//! The agent never speaks to Playwright. It emits symbolic actions naming refs
//! from a snapshot, and this module checks each one before it becomes a message
//! on the sidecar's pipe. The domain allowlist is enforced here, in Rust, which
//! is the authoritative layer: the model is told the rules so it plans within
//! them, and the sidecar blocks requests as a backstop, but this is the check
//! that counts.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{oneshot, Mutex};

use crate::redact::Redactor;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub url: String,
    pub title: String,
    pub tree: String,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub ref_count: i64,
}

/// What the run is allowed to reach on the web.
#[derive(Clone, Debug, Default)]
pub struct DomainPolicy {
    pub allowed: Vec<String>,
    pub strict_network: bool,
}

impl DomainPolicy {
    /// Apex-domain match, with subdomains included implicitly.
    ///
    /// An empty allowlist means nothing is permitted rather than everything.
    /// A task that has not declared where it may go has not been taught yet,
    /// and defaulting that to "anywhere" is the wrong direction to fail.
    pub fn permits(&self, url: &str) -> bool {
        if self.allowed.is_empty() {
            return false;
        }
        let Ok(parsed) = url::Url::parse(url) else {
            return false;
        };
        let Some(host) = parsed.host_str() else {
            return false;
        };
        let host = host.to_ascii_lowercase();
        self.allowed.iter().any(|d| {
            let d = d.trim().to_ascii_lowercase();
            !d.is_empty() && (host == d || host.ends_with(&format!(".{d}")))
        })
    }

    /// Levenshtein guard against a lookalike domain. Catches a redirect to
    /// tennls-club.example when the task is bound to tennis-club.example, which
    /// the plain allowlist already blocks, but this explains *why* it looks
    /// deliberate rather than reporting a generic block.
    pub fn looks_like_typosquat(&self, url: &str) -> Option<String> {
        let parsed = url::Url::parse(url).ok()?;
        let host = parsed.host_str()?.to_ascii_lowercase();
        self.allowed
            .iter()
            .find(|d| {
                let d = d.to_ascii_lowercase();
                let dist = levenshtein(&host, &d);
                dist > 0 && dist <= 2 && !host.ends_with(&format!(".{d}")) && host != d
            })
            .cloned()
    }
}

fn levenshtein(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

struct Pending {
    map: HashMap<u64, oneshot::Sender<std::result::Result<Value, String>>>,
    next_id: u64,
}

/// A live browser, owned by one run.
pub struct Browser {
    stdin: Mutex<ChildStdin>,
    pending: Arc<Mutex<Pending>>,
    child: Mutex<Child>,
    policy: DomainPolicy,
    redactor: Redactor,
}

impl Browser {
    /// Start the sidecar and open a persistent profile.
    pub async fn launch(
        profile_dir: PathBuf,
        policy: DomainPolicy,
        redactor: Redactor,
        headless: bool,
    ) -> Result<Self> {
        let script = sidecar_script()?;
        let node = which_node().ok_or_else(|| {
            anyhow!(
                "Node is needed to run the browser and was not found. Install Node 20 or newer, \
                 or set ERRAND_NODE to its path."
            )
        })?;

        std::fs::create_dir_all(&profile_dir).ok();
        clear_stale_singleton_locks(&profile_dir);

        let mut child = tokio::process::Command::new(&node)
            .arg(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting the browser sidecar with {}", node.display()))?;

        let stdin = child.stdin.take().context("sidecar stdin")?;
        let stdout = child.stdout.take().context("sidecar stdout")?;
        let stderr = child.stderr.take().context("sidecar stderr")?;

        let pending = Arc::new(Mutex::new(Pending {
            map: HashMap::new(),
            next_id: 0,
        }));

        // Sidecar stderr is scrubbed before it reaches the log, because a stack
        // trace is exactly the kind of thing that quotes what it was handling.
        let red = redactor.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                tracing::debug!(target: "browser", "{}", red.scrub(&l));
            }
        });

        let p2 = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let Some(ev) = msg.get("event").and_then(|e| e.as_str()) {
                    tracing::debug!(target: "browser", event = ev, "sidecar event");
                    continue;
                }
                let Some(id) = msg.get("id").and_then(|i| i.as_u64()) else {
                    continue;
                };
                let mut g = p2.lock().await;
                if let Some(tx) = g.map.remove(&id) {
                    let res = if let Some(err) = msg.get("error") {
                        Err(err
                            .get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("browser error")
                            .to_string())
                    } else {
                        Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                    };
                    let _ = tx.send(res);
                }
            }
        });

        let b = Self {
            stdin: Mutex::new(stdin),
            pending,
            child: Mutex::new(child),
            policy: policy.clone(),
            redactor,
        };

        b.call(
            "session.open",
            json!({
                "profile_dir": profile_dir.to_string_lossy(),
                "headless": headless,
                "allowed_domains": policy.allowed,
                "strict_network": policy.strict_network,
            }),
        )
        .await?;

        Ok(b)
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let (tx, rx) = oneshot::channel();
        let id = {
            let mut g = self.pending.lock().await;
            g.next_id += 1;
            let id = g.next_id;
            g.map.insert(id, tx);
            id
        };

        let line = format!(
            "{}\n",
            json!({ "id": id, "method": method, "params": params })
        );
        {
            let mut si = self.stdin.lock().await;
            si.write_all(line.as_bytes()).await?;
            si.flush().await?;
        }

        match tokio::time::timeout(std::time::Duration::from_secs(60), rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(anyhow!(self.redactor.scrub(&e))),
            Ok(Err(_)) => Err(anyhow!("the browser stopped responding")),
            Err(_) => Err(anyhow!("the browser took too long and was given up on")),
        }
    }

    /// Navigate, with the authoritative allowlist check in front of it.
    pub async fn goto(&self, url: &str) -> Result<Snapshot> {
        if !self.policy.permits(url) {
            if let Some(similar) = self.policy.looks_like_typosquat(url) {
                return Err(anyhow!(
                    "Refusing to open {url}. It is not on this task's list of allowed sites, and \
                     it is suspiciously similar to {similar}, which is on the list. That is what a \
                     lookalike phishing domain looks like, so nothing was typed into it."
                ));
            }
            return Err(anyhow!(
                "Refusing to open {url}. This task may only visit: {}. If it genuinely needs \
                 another site, add it to the task rather than working around this.",
                if self.policy.allowed.is_empty() {
                    "nothing yet, because no sites have been approved for it".to_string()
                } else {
                    self.policy.allowed.join(", ")
                }
            ));
        }
        self.call("page.goto", json!({ "url": url })).await?;
        self.snapshot().await
    }

    pub async fn snapshot(&self) -> Result<Snapshot> {
        let v = self.call("page.snapshot", json!({})).await?;
        let mut s: Snapshot = serde_json::from_value(v).context("reading the page snapshot")?;
        // A page can contain a secret we already know, echoed into a field or a
        // confirmation line. Scrub before the model ever sees it.
        s.tree = self.redactor.scrub(&s.tree);
        s.title = self.redactor.scrub(&s.title);
        Ok(s)
    }

    pub async fn act(&self, kind: &str, params: Value) -> Result<Value> {
        let mut p = params;
        p["kind"] = json!(kind);
        self.call("page.act", p).await
    }

    /// Put a secret into a field. The value passes through this function and
    /// nowhere else: not into the journal, not into a prompt, not into an error.
    pub async fn fill_secret(&self, r#ref: &str, secret: &str, label: &str) -> Result<()> {
        self.redactor.register(secret, label);
        self.call(
            "secure.fill",
            json!({ "ref": r#ref, "value": secret, "label": label }),
        )
        .await
        .map(|_| ())
    }

    pub async fn screenshot_to(&self, path: &std::path::Path) -> Result<()> {
        self.call(
            "page.screenshot",
            json!({ "mask_secure": true, "path": path.to_string_lossy() }),
        )
        .await
        .map(|_| ())
    }

    pub async fn detect_captcha(&self) -> Result<Option<String>> {
        let v = self.call("captcha.detect", json!({})).await?;
        Ok(v.get("captcha")
            .and_then(|c| c.as_str())
            .map(str::to_string))
    }

    pub async fn close(&self) {
        let _ = self
            .call("session.close", json!({ "save_state": true }))
            .await;
        let mut c = self.child.lock().await;
        let _ = c.kill().await;
    }
}

/// A Chromium profile serves one process at a time. A hard kill leaves lock
/// files behind that make the next launch fail with a cryptic error, so they
/// are cleared when nothing live owns them.
fn clear_stale_singleton_locks(profile_dir: &std::path::Path) {
    for name in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
        let p = profile_dir.join(name);
        if p.exists() {
            let _ = std::fs::remove_file(&p);
        }
    }
}

fn which_node() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ERRAND_NODE") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    for c in [
        "/opt/homebrew/bin/node",
        "/usr/local/bin/node",
        "/usr/bin/node",
    ] {
        let p = PathBuf::from(c);
        if p.exists() {
            return Some(p);
        }
    }
    std::process::Command::new("which")
        .arg("node")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
        .filter(|p| p.exists())
}

fn sidecar_script() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("ERRAND_SIDECAR") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Ok(p);
        }
    }
    // Bundled next to the binary in a shipped build; in the repo during dev.
    let mut candidates = vec![];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("browser-agent/src/main.mjs"));
            candidates.push(dir.join("../Resources/browser-agent/src/main.mjs"));
        }
    }
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sidecars/browser-agent/src/main.mjs"),
    );

    candidates
        .into_iter()
        .find(|p| p.exists())
        .ok_or_else(|| anyhow!("could not find the browser sidecar script"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(domains: &[&str]) -> DomainPolicy {
        DomainPolicy {
            allowed: domains.iter().map(|s| s.to_string()).collect(),
            strict_network: false,
        }
    }

    #[test]
    fn an_empty_allowlist_permits_nothing() {
        // A task that has not said where it may go has not been taught yet.
        let p = policy(&[]);
        assert!(!p.permits("https://example.com/"));
    }

    #[test]
    fn subdomains_are_included_but_lookalikes_are_not() {
        let p = policy(&["tennis-club.example"]);
        assert!(p.permits("https://tennis-club.example/book"));
        assert!(p.permits("https://booking.tennis-club.example/x"));
        assert!(!p.permits("https://tennis-club.example.evil.com/"));
        assert!(!p.permits("https://tennisclub.example/"));
    }

    #[test]
    fn a_near_miss_domain_is_reported_as_a_lookalike() {
        let p = policy(&["tennis-club.example"]);
        // One character different: the classic phishing shape.
        assert_eq!(
            p.looks_like_typosquat("https://tennls-club.example/login"),
            Some("tennis-club.example".to_string())
        );
        assert!(p
            .looks_like_typosquat("https://entirely-different.com/")
            .is_none());
    }

    #[test]
    fn a_legitimate_subdomain_is_not_flagged_as_a_lookalike() {
        let p = policy(&["tennis-club.example"]);
        assert!(p
            .looks_like_typosquat("https://booking.tennis-club.example/")
            .is_none());
    }

    #[test]
    fn rubbish_urls_are_refused_rather_than_parsed_loosely() {
        let p = policy(&["example.com"]);
        assert!(!p.permits("not a url"));
        assert!(!p.permits("javascript:alert(1)"));
        assert!(!p.permits("file:///etc/passwd"));
    }
}

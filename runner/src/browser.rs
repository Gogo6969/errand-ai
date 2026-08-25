//! Driving the browser, from the side the model does not touch.
//!
//! The agent never speaks to Playwright. It emits symbolic actions naming refs
//! from a snapshot, and this module checks each one before it becomes a message
//! on the sidecar's pipe. The domain allowlist is enforced here, in Rust, which
//! is the authoritative layer: the model is told the rules so it plans within
//! them, and the sidecar blocks requests as a backstop, but this is the check
//! that counts.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
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

/// Pull `role "label" [ref=eN]` out of a snapshot line.
fn parse_refs(tree: &str) -> HashMap<String, (String, String)> {
    let mut out = HashMap::new();
    for line in tree.lines() {
        let Some(rs) = line.find("[ref=") else {
            continue;
        };
        let Some(re) = line[rs..].find(']') else {
            continue;
        };
        let r#ref = line[rs + 5..rs + re].to_string();

        let body = line.trim_start().trim_start_matches("- ");
        let role = body.split_whitespace().next().unwrap_or("").to_string();
        let label = match (body.find('"'), body.rfind('"')) {
            (Some(a), Some(b)) if b > a => body[a + 1..b].to_string(),
            _ => String::new(),
        };
        // Carried through the role so the classifier can see it without
        // widening the map's shape.
        let role = if line.contains("[submit]") {
            format!("{role}+submit")
        } else {
            role
        };
        out.insert(r#ref, (role, label));
    }
    out
}

/// Does this control look like it does something that cannot be undone?
///
/// Judged from what the control actually says, independently of the model's own
/// account of what it is doing. The two are combined by taking the stricter,
/// because the expensive mistake here is one-directional: a needless
/// confirmation costs a moment, an unguarded booking costs a court.
/// As above, but told whether the control submits a form.
///
/// A link that merely navigates is not a commitment, however it is worded. This
/// distinction was learned from a real site: a homepage link reading "Book a
/// court" was classified as a booking, which armed the fence and then blocked
/// the actual booking button on the next page. Words alone are not enough.
pub fn classify(role: &str, label: &str, submits: bool) -> Option<&'static str> {
    let l = label.to_ascii_lowercase();

    const PURCHASE: &[&str] = &[
        "pay",
        "buy",
        "purchase",
        "checkout",
        "place order",
        "subscribe",
    ];
    const DELETION: &[&str] = &[
        "delete",
        "remove",
        "cancel booking",
        "cancel reservation",
        "unsubscribe",
        "close account",
    ];
    // These commit something only when the control actually does something,
    // rather than taking you to the page where you might.
    const BOOKING: &[&str] = &["book", "reserve", "confirm", "submit", "apply", "register"];
    const MESSAGE: &[&str] = &["send", "post", "publish", "reply"];

    let hit = |set: &[&str]| set.iter().any(|w| l.contains(w));

    let acts = match role {
        // A button, or something behaving as one, can commit.
        "button" | "menuitem" | "" => true,
        // A link navigates. Only the words that are never navigation count.
        "link" => {
            return if hit(PURCHASE) || hit(DELETION) {
                Some(if hit(PURCHASE) {
                    "purchase"
                } else {
                    "deletion"
                })
            } else {
                None
            }
        }
        // Text boxes, checkboxes and the rest commit nothing by being clicked.
        _ => false,
    };
    if !acts && !submits {
        return None;
    }

    if hit(PURCHASE) {
        Some("purchase")
    } else if hit(DELETION) {
        Some("deletion")
    } else if hit(BOOKING) {
        Some("booking")
    } else if hit(MESSAGE) {
        Some("message")
    } else if submits && l.len() < 40 {
        // A form submission whose wording we do not recognise. Treated as a
        // commitment, because the cost of a needless confirmation is a moment
        // and the cost of the other mistake is a real booking.
        Some("form_submit")
    } else {
        None
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
    /// Set once the sidecar process has ended, holding the sentence to give
    /// anyone still waiting and anyone who calls afterwards. Without it a dead
    /// helper is indistinguishable from a slow one: the caller waits out the
    /// full timeout and is told the browser took too long, which is the wrong
    /// thing to say about a process that was never there.
    gone: Option<String>,
}

/// How much of the sidecar's stderr to keep. It writes its reason for dying in
/// a sentence or two; the rest is a stack trace nobody needs quoted back.
const STDERR_TAIL_LINES: usize = 12;

/// A live browser, owned by one run.
pub struct Browser {
    /// Label and role of every ref in the most recent snapshot, so an action
    /// can be classified by what it actually says before it is performed.
    /// The model's own account of what it is clicking is not trusted alone.
    last_refs: Mutex<HashMap<String, (String, String)>>,
    stdin: Mutex<ChildStdin>,
    pending: Arc<Mutex<Pending>>,
    /// Shared with the reader task, which needs the exit status to explain a
    /// death and must not hold the lock while `close` is trying to kill.
    child: Arc<Mutex<Child>>,
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
        let node = which_node().ok_or_else(|| anyhow!(NODE_MISSING_HELP))?;
        Self::launch_with(&node, &script, profile_dir, policy, redactor, headless).await
    }

    /// As `launch`, with the runtime and the script named rather than searched
    /// for. Split out so the search and the supervision can be exercised
    /// separately.
    async fn launch_with(
        node: &Path,
        script: &Path,
        profile_dir: PathBuf,
        policy: DomainPolicy,
        redactor: Redactor,
        headless: bool,
    ) -> Result<Self> {
        std::fs::create_dir_all(&profile_dir).ok();
        clear_stale_singleton_locks(&profile_dir);

        let mut child = tokio::process::Command::new(node)
            .arg(script)
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
            gone: None,
        }));
        let child = Arc::new(Mutex::new(child));

        // Sidecar stderr is scrubbed before it reaches the log, because a stack
        // trace is exactly the kind of thing that quotes what it was handling.
        // The tail is kept as well as logged: if node dies, this is the only
        // place its reason exists, and the log is not where the person who is
        // waiting for their task will look.
        let stderr_tail: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let red = redactor.clone();
        let tail = stderr_tail.clone();
        let reading_stderr = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                let l = red.scrub(&l);
                tracing::debug!(target: "browser", "{}", l);
                let mut t = tail.lock().await;
                if t.len() == STDERR_TAIL_LINES {
                    t.remove(0);
                }
                t.push(l);
            }
        });

        let p2 = pending.clone();
        let child2 = child.clone();
        let tail2 = stderr_tail.clone();
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

            // Stdout has closed, which for a child we own means node has gone.
            // Everything below exists to turn that into a sentence rather than
            // leaving callers parked until their timeout.
            //
            // Stderr first: both pipes close at once and the reason for dying
            // is on the other one, so a moment is spent waiting for it rather
            // than reporting a death with nothing to say about it.
            let _ = tokio::time::timeout(Duration::from_secs(2), reading_stderr).await;
            let code = {
                let mut c = child2.lock().await;
                tokio::time::timeout(Duration::from_secs(5), c.wait())
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .and_then(|s| s.code())
            };
            let said = tail2.lock().await.clone();
            let msg = describe_sidecar_exit(code, &said);

            // Marking and draining under one lock, so a call that arrives in
            // the middle either gets drained here or sees the marker. A reason
            // already there is `close` saying it meant to do this, and a
            // deliberate shutdown is not worth a warning.
            let mut g = p2.lock().await;
            let reason = g
                .gone
                .get_or_insert_with(|| {
                    tracing::warn!(target: "browser", "{msg}");
                    msg
                })
                .clone();
            for (_, tx) in g.map.drain() {
                let _ = tx.send(Err(reason.clone()));
            }
        });

        let b = Self {
            last_refs: Mutex::new(HashMap::new()),
            stdin: Mutex::new(stdin),
            pending,
            child,
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
            // Already dead. Say so now rather than spending a minute finding out.
            if let Some(reason) = &g.gone {
                return Err(anyhow!(reason.clone()));
            }
            g.next_id += 1;
            let id = g.next_id;
            g.map.insert(id, tx);
            id
        };

        let line = format!(
            "{}\n",
            json!({ "id": id, "method": method, "params": params })
        );
        let sent = {
            let mut si = self.stdin.lock().await;
            match si.write_all(line.as_bytes()).await {
                Ok(()) => si.flush().await,
                Err(e) => Err(e),
            }
        };
        if let Err(e) = sent {
            // The pipe only breaks when the far end has gone, and at this
            // instant the reader task is working out why. Waiting for it is
            // the difference between naming the real fault and reporting a
            // broken pipe, which is true and useless.
            let reason = reason_it_went(&self.pending, Duration::from_secs(3)).await;
            let mut g = self.pending.lock().await;
            g.map.remove(&id);
            return Err(anyhow!(reason.unwrap_or_else(|| format!(
                "The browser helper is no longer listening, so this step was never carried out \
                 and nothing on any website was changed. The underlying problem was: {e}"
            ))));
        }

        match tokio::time::timeout(Duration::from_secs(60), rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(anyhow!(self.redactor.scrub(&e))),
            Ok(Err(_)) => Err(anyhow!("the browser stopped responding")),
            Err(_) => {
                // Nobody is going to answer this one now; do not leave the
                // sender parked in the map for the life of the run.
                self.pending.lock().await.map.remove(&id);
                Err(anyhow!("the browser took too long and was given up on"))
            }
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

    /// What a ref says, from the last snapshot: (role, label).
    pub async fn describe_ref(&self, r#ref: &str) -> Option<(String, String)> {
        self.last_refs.lock().await.get(r#ref).cloned()
    }

    pub async fn snapshot(&self) -> Result<Snapshot> {
        let v = self.call("page.snapshot", json!({})).await?;
        let mut s: Snapshot = serde_json::from_value(v).context("reading the page snapshot")?;
        *self.last_refs.lock().await = parse_refs(&s.tree);
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
        // Say why before killing, so the watcher does not report a shutdown we
        // asked for as a crash, in the log or to anyone still waiting.
        self.pending
            .lock()
            .await
            .gone
            .get_or_insert_with(|| "The browser was closed at the end of the run.".to_string());
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

/// What to say when there is no Node at all.
///
/// Written once and shared with `doctor`, so a failing run and a health check
/// cannot disagree about where Errand looked or what to do about it.
pub const NODE_MISSING_HELP: &str =
    "Node is needed to open web pages, and Errand could not find it. It looked for a copy \
     bundled with the app, then in Volta, nvm and fnm, then in /opt/homebrew/bin, /usr/local/bin \
     and /usr/bin, then on PATH. Install Node 20 or newer from https://nodejs.org, or set \
     ERRAND_NODE to the full path of a node you already have. Nothing else stops working: tasks \
     that do not open a web page still run.";

/// Sort key for a directory named like `v20.11.0`.
///
/// Compared as numbers rather than text, because as text "v10.0.0" sorts
/// before "v9.0.0" and a machine with both quietly runs the older one. Parsing
/// stops at the first part that is not a number, so a release beats its own
/// release candidate and a directory that is not a version sorts last.
fn version_key(name: &str) -> Vec<u64> {
    name.trim_start_matches('v')
        .split('.')
        .map_while(|part| part.parse::<u64>().ok())
        .collect()
}

/// The newest version directory under `root` that actually contains `rel`.
///
/// A half-removed version leaves its directory behind, so the file being there
/// is what counts, not the directory name.
fn newest_installed(root: &Path, rel: &str) -> Option<PathBuf> {
    let mut found: Vec<(Vec<u64>, PathBuf)> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let exe = e.path().join(rel);
            exe.exists()
                .then(|| (version_key(&e.file_name().to_string_lossy()), exe))
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().next().map(|(_, p)| p)
}

/// Everywhere node might be, in the order we prefer it.
///
/// The system directories alone were not enough: nvm, fnm and Volta all
/// install outside every one of them, so a Mac with a perfectly good node was
/// being told it had none and the browser never opened.
fn node_candidates(exe_dir: Option<&Path>, home: Option<&Path>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    // A runtime shipped inside the app wins, because it is the one this
    // version was tested against.
    if let Some(d) = exe_dir {
        out.push(d.join("node"));
    }
    if let Some(h) = home {
        out.push(h.join(".volta/bin/node"));
        // nvm and fnm keep one directory per installed version, so the newest
        // has to be found rather than named.
        out.extend(newest_installed(&h.join(".nvm/versions/node"), "bin/node"));
        out.extend(newest_installed(
            &h.join(".local/share/fnm/node-versions"),
            "installation/bin/node",
        ));
    }
    out.extend(
        [
            "/opt/homebrew/bin/node",
            "/usr/local/bin/node",
            "/usr/bin/node",
        ]
        .map(PathBuf::from),
    );
    out
}

pub fn which_node() -> Option<PathBuf> {
    // The override stays ahead of everything, the bundled runtime included. It
    // is the one thing a person can do by hand when this search is wrong, and
    // an escape hatch that loses to a default is not an escape hatch.
    if let Ok(p) = std::env::var("ERRAND_NODE") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf));
    node_candidates(exe_dir.as_deref(), dirs::home_dir().as_deref())
        .into_iter()
        .find(|p| p.exists())
        .or_else(|| {
            // PATH last: under launchd it is whatever the plist says, which is
            // exactly the case the list above exists to cover.
            std::process::Command::new("which")
                .arg("node")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
                .filter(|p| p.exists())
        })
}

/// Wait a moment for the reader task to establish why the sidecar went.
///
/// Polled rather than signalled because this is a once-per-failure path and a
/// notifier here would be more machinery than the problem deserves.
async fn reason_it_went(pending: &Mutex<Pending>, within: Duration) -> Option<String> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if let Some(reason) = pending.lock().await.gone.clone() {
            return Some(reason);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// What to tell someone when the sidecar ends while a call is in flight.
///
/// An exit code on its own means nothing to anybody, so whatever the process
/// said on its way out is carried through: it writes its own plain-English
/// reason to stderr before it goes, and that sentence is the useful part.
fn describe_sidecar_exit(code: Option<i32>, said: &[String]) -> String {
    let mut msg = String::from(
        "The browser helper stopped before it could answer, so no page was opened and nothing on \
         any website was changed. ",
    );
    match code {
        Some(0) => msg.push_str("It closed itself without reporting a problem. "),
        Some(c) => msg.push_str(&format!("It exited with code {c}. ")),
        None => msg.push_str("It was stopped before it could report a code. "),
    }
    let said: Vec<&str> = said
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    if said.is_empty() {
        msg.push_str(
            "It gave no reason, which usually means its installation is incomplete. \
             Reinstalling Errand should put it back.",
        );
    } else {
        msg.push_str("It said: ");
        msg.push_str(&said.join(" / "));
    }
    msg
}

/// What the sidecar reports about the browser it would drive.
#[derive(Debug, Default, Deserialize)]
pub struct BrowserProbe {
    #[serde(default)]
    pub found: bool,
    /// The browser's name, when there is one.
    #[serde(default)]
    pub name: Option<String>,
    /// What to do about there being none. Written by the sidecar, which is the
    /// layer that knows where it looked.
    #[serde(default)]
    pub message: Option<String>,
}

/// A health check must not be able to hang. If the sidecar has not answered a
/// question this cheap by now, saying so is more use than waiting longer.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Ask the sidecar which browser it would drive, without opening one.
///
/// This is `doctor`'s way of answering the question before a task depends on
/// the answer: start node, ask, stop. Nothing is launched and no profile is
/// touched.
pub async fn probe_browser(node: &Path, script: &Path) -> Result<BrowserProbe> {
    let mut child = tokio::process::Command::new(node)
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting the browser helper with {}", node.display()))?;

    let mut stdin = child.stdin.take().context("sidecar stdin")?;
    let stdout = child.stdout.take().context("sidecar stdout")?;
    let stderr = child.stderr.take().context("sidecar stderr")?;

    let collecting = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut out: Vec<String> = Vec::new();
        while let Ok(Some(l)) = lines.next_line().await {
            if out.len() == STDERR_TAIL_LINES {
                out.remove(0);
            }
            out.push(l.chars().take(300).collect());
        }
        out
    });

    let asked = tokio::time::timeout(PROBE_TIMEOUT, async {
        // stdin is held open until the answer arrives: closing it is the
        // sidecar's signal to shut down, and it would race the reply.
        stdin
            .write_all(b"{\"id\":1,\"method\":\"browser.probe\",\"params\":{}}\n")
            .await?;
        stdin.flush().await?;
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines.next_line().await? {
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if msg.get("id").and_then(Value::as_u64) != Some(1) {
                continue;
            }
            if let Some(m) = msg
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
            {
                bail!("the browser helper could not answer: {m}");
            }
            let result = msg.get("result").cloned().unwrap_or(Value::Null);
            return serde_json::from_value::<BrowserProbe>(result)
                .context("reading the browser helper's answer");
        }
        bail!("the browser helper stopped without answering")
    })
    .await;

    let _ = child.kill().await;
    // Killing closes its end of stderr, so whatever it had already written is
    // still readable and the collector finishes on its own.
    let said = tokio::time::timeout(Duration::from_secs(2), collecting)
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default();
    let detail = if said.is_empty() {
        String::new()
    } else {
        format!(". It said: {}", said.join(" / "))
    };

    match asked {
        Ok(Ok(p)) => Ok(p),
        Ok(Err(e)) => Err(anyhow!("{e}{detail}")),
        Err(_) => Err(anyhow!(
            "the browser helper did not answer within {} seconds{detail}",
            PROBE_TIMEOUT.as_secs()
        )),
    }
}

pub fn sidecar_script() -> Result<PathBuf> {
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
        .ok_or_else(|| anyhow!(SIDECAR_MISSING_HELP))
}

/// Shared with `doctor` for the same reason as `NODE_MISSING_HELP`.
pub const SIDECAR_MISSING_HELP: &str =
    "Errand could not find the browser helper that opens web pages. It looks next to its own \
     binary and in the app's Resources folder. This means part of the installation is missing, \
     and reinstalling Errand should put it back. If you keep it somewhere else, set \
     ERRAND_SIDECAR to the full path of browser-agent/src/main.mjs. Tasks that do not open a web \
     page are unaffected.";

#[cfg(test)]
mod tests {
    use super::*;

    /// A click on something that does not submit a form.
    fn looks_irreversible(role: &str, label: &str) -> Option<&'static str> {
        classify(role, label, false)
    }

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

    #[test]
    fn refs_are_parsed_out_of_a_snapshot() {
        let tree = "- heading \"Courts\"\n  - button \"Book Wednesday 19:00\" [ref=e3]\n  - textbox \"Email\" [ref=e1]";
        let m = parse_refs(tree);
        assert_eq!(
            m.get("e3"),
            Some(&("button".to_string(), "Book Wednesday 19:00".to_string()))
        );
        assert_eq!(m.get("e1").unwrap().0, "textbox");
    }

    #[test]
    fn buttons_that_commit_something_are_recognised() {
        assert_eq!(
            looks_irreversible("button", "Book this court"),
            Some("booking")
        );
        assert_eq!(looks_irreversible("button", "Pay now"), Some("purchase"));
        assert_eq!(
            looks_irreversible("button", "Confirm booking"),
            Some("booking")
        );
        assert_eq!(
            looks_irreversible("button", "Delete my account"),
            Some("deletion")
        );
        assert_eq!(
            looks_irreversible("button", "Send message"),
            Some("message")
        );
    }

    #[test]
    fn ordinary_navigation_is_not_treated_as_irreversible() {
        assert_eq!(looks_irreversible("link", "Back to courts"), None);
        assert_eq!(looks_irreversible("button", "Next page"), None);
        assert_eq!(
            looks_irreversible("textbox", "Book"),
            None,
            "typing is not committing"
        );
    }

    #[test]
    fn purchase_outranks_booking_when_a_label_suggests_both() {
        // "Confirm and pay" must be classified by the more serious of the two.
        assert_eq!(
            looks_irreversible("button", "Confirm and pay"),
            Some("purchase")
        );
    }

    #[test]
    fn a_navigation_link_is_not_a_commitment_however_it_is_worded() {
        // Learned from a real site: a homepage link reading "Book a court"
        // armed the fence and then blocked the actual booking button.
        assert_eq!(looks_irreversible("link", "Book a court"), None);
        assert_eq!(looks_irreversible("link", "Reserve a slot"), None);
        assert_eq!(looks_irreversible("link", "Send us feedback"), None);
        // But a link that plainly spends money or destroys something still counts.
        assert_eq!(looks_irreversible("link", "Buy now"), Some("purchase"));
        assert_eq!(
            looks_irreversible("link", "Delete account"),
            Some("deletion")
        );
    }

    #[test]
    fn a_button_that_books_is_still_a_commitment() {
        assert_eq!(
            looks_irreversible("button", "Book Wednesday 19:00 court 2"),
            Some("booking")
        );
        assert_eq!(
            looks_irreversible("button", "Confirm booking"),
            Some("booking")
        );
    }

    #[test]
    fn an_unrecognised_form_submission_is_treated_as_a_commitment() {
        // The wording is unfamiliar, but it submits a form, so it errs safe.
        assert_eq!(classify("button", "Continue", true), Some("form_submit"));
        // The same word on a plain link commits nothing.
        assert_eq!(classify("link", "Continue", false), None);
    }

    /// A throwaway directory to stand in for a home folder. Named after the
    /// test so two of them cannot collide when the suite runs in parallel.
    fn scratch(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("errand-node-search-{name}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "").unwrap();
    }

    #[test]
    fn a_node_installed_by_nvm_is_found_and_the_newest_version_wins() {
        let home = scratch("nvm");
        touch(&home.join(".nvm/versions/node/v9.11.2/bin/node"));
        touch(&home.join(".nvm/versions/node/v10.0.0/bin/node"));

        let looked = node_candidates(None, Some(&home));
        assert!(
            looked.contains(&home.join(".nvm/versions/node/v10.0.0/bin/node")),
            "a machine with nvm has node; it must not be told it has none: {looked:?}"
        );
        assert!(
            !looked.contains(&home.join(".nvm/versions/node/v9.11.2/bin/node")),
            "v10 is newer than v9, whatever the two do when sorted as text"
        );

        let nvm = looked
            .iter()
            .position(|p| p.starts_with(home.join(".nvm")))
            .unwrap();
        let system = looked
            .iter()
            .position(|p| p.starts_with("/usr"))
            .unwrap_or(usize::MAX);
        assert!(nvm < system, "the version manager's node is the chosen one");

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_node_installed_by_fnm_or_volta_is_found_where_each_one_puts_it() {
        let home = scratch("fnm-volta");
        touch(&home.join(".local/share/fnm/node-versions/v22.11.0/installation/bin/node"));

        let looked = node_candidates(None, Some(&home));
        assert!(looked
            .contains(&home.join(".local/share/fnm/node-versions/v22.11.0/installation/bin/node")));
        assert!(
            looked.contains(&home.join(".volta/bin/node")),
            "Volta's shim is a fixed path, so it is always worth trying"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_half_removed_node_version_is_not_offered_as_the_newest() {
        // The directory outlives the uninstall; the binary does not.
        let home = scratch("stale");
        std::fs::create_dir_all(home.join(".nvm/versions/node/v24.0.0")).unwrap();
        touch(&home.join(".nvm/versions/node/v20.9.0/bin/node"));

        let looked = node_candidates(None, Some(&home));
        assert!(looked.contains(&home.join(".nvm/versions/node/v20.9.0/bin/node")));
        assert!(!looked.contains(&home.join(".nvm/versions/node/v24.0.0/bin/node")));

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_node_shipped_inside_the_app_is_tried_before_anything_on_the_machine() {
        let looked = node_candidates(
            Some(Path::new("/Applications/Errand-AI.app/Contents/MacOS")),
            Some(Path::new("/no-such-home")),
        );
        assert_eq!(
            looked.first().unwrap(),
            Path::new("/Applications/Errand-AI.app/Contents/MacOS/node")
        );
    }

    #[tokio::test]
    async fn a_browser_helper_that_dies_at_startup_fails_the_call_instead_of_waiting_a_minute() {
        // /bin/sh rather than node, because what is being tested is a helper
        // process that exits before answering, and this makes that happen the
        // same way on every machine. Node with no node_modules is the real
        // version of it.
        let dir = scratch("dead-helper");
        let script = dir.join("dies.sh");
        std::fs::write(
            &script,
            "echo \"Cannot find module 'playwright-core'\" >&2\nexit 3\n",
        )
        .unwrap();

        let started = std::time::Instant::now();
        let outcome = Browser::launch_with(
            Path::new("/bin/sh"),
            &script,
            dir.join("profile"),
            DomainPolicy::default(),
            Redactor::default(),
            true,
        )
        .await;
        let Err(err) = outcome else {
            panic!("a helper that exited cannot have opened a browser");
        };
        let err = err.to_string();

        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the old behaviour was a full 60-second wait for a process that had already gone: \
             took {:?}",
            started.elapsed()
        );
        assert!(
            err.contains("Cannot find module"),
            "what it said on its way out is the only useful part: {err}"
        );
        assert!(
            !err.contains("too long"),
            "it was not slow, it was dead: {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sidecar_that_dies_at_birth_is_reported_as_dead_rather_than_slow() {
        let msg = describe_sidecar_exit(
            Some(1),
            &["fatal (startup): Errand could not start its browser helper.".to_string()],
        );
        assert!(
            msg.contains("could not start its browser helper"),
            "the reason it gave is the useful part: {msg}"
        );
        assert!(msg.contains("code 1"), "{msg}");
        assert!(
            !msg.contains("too long"),
            "a process that was already dead was never slow: {msg}"
        );
        assert!(
            msg.contains("nothing on any website was changed"),
            "say what did not happen, which is what the worry is about: {msg}"
        );
    }

    #[test]
    fn a_sidecar_that_dies_without_a_word_still_gets_a_fix_rather_than_a_shrug() {
        let msg = describe_sidecar_exit(None, &["   ".to_string()]);
        assert!(msg.contains("Reinstalling Errand"), "{msg}");
    }

    #[test]
    fn the_submit_marker_is_carried_through_from_the_snapshot() {
        let tree = "- button \"Continue\" [ref=e4] [submit]\n- link \"Back\" [ref=e5]";
        let m = parse_refs(tree);
        assert_eq!(m.get("e4").unwrap().0, "button+submit");
        assert_eq!(m.get("e5").unwrap().0, "link");
    }
}

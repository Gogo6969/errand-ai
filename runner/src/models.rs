//! Actually asking a model something.
//!
//! One function, `ask`, which takes a role rather than a model. Callers say
//! what kind of question it is and this works out where to send it, falls back
//! when something is down, and reports which model answered so a run can say so
//! rather than leaving you guessing.

use errand_core::providers::{Kind, Provider, Role};
use serde_json::{json, Value};

use crate::state::AppState;

/// What answered, and what it said.
#[derive(Debug, Clone)]
pub struct Answer {
    pub text: String,
    /// Shown in the run, so you can tell what actually did the thinking.
    pub provider_label: String,
    pub model: String,
    /// True when nothing left this machine.
    pub was_local: bool,
}

/// Ask the model bound to a role.
///
/// Walks the chain in order, so a local model being switched off falls through
/// to the next rather than failing the run.
pub async fn ask(state: &AppState, role: Role, prompt: &str) -> anyhow::Result<Answer> {
    let providers = errand_core::db::list_providers(state.pool()).await?;
    let bindings = errand_core::db::list_role_bindings(state.pool()).await?;
    let local_only = errand_core::db::get_setting(state.pool(), "privacy.local_only")
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let chain = errand_core::providers::resolve_chain(&providers, &bindings, role, local_only);
    if chain.is_empty() {
        anyhow::bail!(errand_core::providers::explain_empty_chain(
            role, local_only, &providers
        ));
    }

    let mut last: Option<String> = None;
    for p in chain {
        let model = p
            .model
            .clone()
            .unwrap_or_else(|| errand_core::providers::default_model_for(role).to_string());

        let result = match p.kind_enum() {
            Some(Kind::ClaudeCli) => crate::executor::ask_model(prompt, &model, 4)
                .await
                .map_err(|e| e.to_string()),
            Some(Kind::OpenAiCompat) => {
                let key = key_for(&p.id).await;
                ask_openai_compatible(
                    p.base_url.as_deref().unwrap_or(""),
                    &model,
                    prompt,
                    key.as_deref(),
                )
                .await
            }
            Some(Kind::AnthropicApi) => ask_anthropic(&model, prompt).await,
            None => Err("that provider has an unknown type".into()),
        };

        match result {
            Ok(text) if !text.trim().is_empty() => {
                return Ok(Answer {
                    text,
                    provider_label: p.label.clone(),
                    model,
                    was_local: p.is_local(),
                })
            }
            Ok(_) => {
                last = Some(format!("{} returned nothing", p.label));
            }
            Err(e) => {
                tracing::warn!(provider = %p.label, "could not use this model: {e}");
                last = Some(format!("{}: {e}", p.label));
            }
        }
    }

    anyhow::bail!(
        "None of the models Errand could reach answered. Last problem: {}",
        last.unwrap_or_else(|| "unknown".into())
    )
}

/// The models that could carry out a task, best first.
///
/// Not `resolve_chain(Role::Executor)`, which asks each provider whether it
/// "can carry out tasks" and gets back a no from everything except the Claude
/// command line tool. That answer was true only because there was one agent
/// loop and it lived inside the CLI. There are two now, and Errand owns the
/// tools, the budget and the fence in both, so the question here is which loop
/// to use rather than whether to refuse.
///
/// The person's own choice comes first, then anything else that is switched on,
/// so a machine that is asleep does not stop a run before it starts.
pub async fn executor_chain(state: &AppState) -> anyhow::Result<Vec<Provider>> {
    let providers = errand_core::db::list_providers(state.pool()).await?;
    let bindings = errand_core::db::list_role_bindings(state.pool()).await?;
    let local_only = errand_core::db::get_setting(state.pool(), "privacy.local_only")
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut chain: Vec<Provider> = vec![];
    for (role, id) in &bindings {
        if *role != Role::Executor {
            continue;
        }
        if let Some(p) = providers.iter().find(|p| &p.id == id) {
            if p.enabled {
                chain.push(p.clone());
            }
        }
    }
    for p in &providers {
        if !p.enabled || chain.iter().any(|c| c.id == p.id) {
            continue;
        }
        chain.push(p.clone());
    }
    if local_only {
        chain.retain(|p| p.is_local());
    }

    if chain.is_empty() {
        if local_only {
            anyhow::bail!(
                "This task is set to stay on your own machine, and no model on this machine is \
                 switched on. Add one in Settings under Models, or turn that setting off for this \
                 task."
            );
        }
        anyhow::bail!(
            "Errand has no model switched on to carry out this task. Open Settings and either \
             install the Claude command line tool and run 'claude /login' once, or add a model of \
             your own by address and choose it for \"Doing the task\"."
        );
    }
    Ok(chain)
}

/// Anything speaking the OpenAI chat format.
///
/// This is the whole point of the module: OpenAI, Google, OpenRouter, xAI,
/// DeepSeek, Moonshot, Mistral, Groq, Z.ai, Together, Fireworks, Cerebras,
/// Perplexity, Ollama, LM Studio, vLLM and llama.cpp all speak it, so one
/// adapter reaches every one of them and adding another is a row in a list
/// rather than a new code path.
///
/// The key is passed in rather than looked up here, so this function never
/// touches the keychain and can be tested against a stub without one.
pub async fn ask_openai_compatible(
    base_url: &str,
    model: &str,
    prompt: &str,
    key: Option<&str>,
) -> Result<String, String> {
    let body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": prompt }],
        "temperature": 0.2,
        "stream": false,
    });
    let v = post_chat(base_url, key, &body, false)
        .await
        .map_err(|e| e.message)?;
    Ok(v["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string())
}

/// Why a chat request did not come back with an answer.
///
/// The tool-calling case is separated out because the fix is a different one:
/// a model that cannot call tools is not a network problem or a bad key, and
/// telling somebody to check their connection when they need to pick another
/// model wastes their evening.
#[derive(Debug, Clone)]
pub struct ChatError {
    pub message: String,
    pub no_tool_support: bool,
}

impl ChatError {
    fn plain(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            no_tool_support: false,
        }
    }
}

impl std::fmt::Display for ChatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ChatError {}

/// One assistant turn, in the only two forms that matter: what it said, and
/// what it wants run.
#[derive(Debug, Clone, Default)]
pub struct Turn {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
}

/// One tool call, with its arguments already unpacked.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    /// The original text, when the server sent arguments that were not
    /// readable JSON. Kept so the caller can tell the model what it sent
    /// rather than dispatching a guess.
    pub unreadable: Option<String>,
}

impl ToolCall {
    /// The arguments as they go back into the conversation, which is always a
    /// string: that is the shape every server accepts, whichever shape it sent.
    fn arguments_text(&self) -> String {
        match &self.unreadable {
            Some(raw) => raw.clone(),
            None => self.arguments.to_string(),
        }
    }
}

impl Turn {
    /// This turn as a message to append to the conversation.
    ///
    /// Rebuilt rather than echoed verbatim, so the ids in the assistant message
    /// are the same ones the tool replies will quote even where the server sent
    /// no ids at all.
    pub fn as_message(&self) -> Value {
        let mut m = json!({ "role": "assistant", "content": self.text });
        if !self.tool_calls.is_empty() {
            m["tool_calls"] = Value::Array(
                self.tool_calls
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c.id,
                            "type": "function",
                            "function": { "name": c.name, "arguments": c.arguments_text() }
                        })
                    })
                    .collect(),
            );
        }
        m
    }
}

/// A conversation with tools, for anything speaking the OpenAI chat format.
///
/// The same wire as `ask_openai_compatible` and deliberately so, but this one
/// hands over a tool list and gives back whatever the model wants run, so the
/// caller can execute it and come round again. That loop is what makes a model
/// able to carry out a task rather than only answer a question.
pub async fn chat_with_tools(
    base_url: &str,
    model: &str,
    messages: &[Value],
    tools: &[Value],
    key: Option<&str>,
) -> Result<Turn, ChatError> {
    let mut body = json!({
        "model": model,
        "messages": messages,
        "temperature": 0.2,
        "stream": false,
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools.to_vec());
        body["tool_choice"] = json!("auto");
    }
    let v = post_chat(base_url, key, &body, !tools.is_empty()).await?;
    Ok(read_turn(&v["choices"][0]["message"]))
}

/// One POST to a chat endpoint, with the error wording both callers share.
///
/// `wants_tools` only changes what a failure says: when tools were sent, a
/// refusal is worth quoting, because the server's own sentence is usually the
/// thing that names the model that cannot do it.
async fn post_chat(
    base_url: &str,
    key: Option<&str>,
    body: &Value,
    wants_tools: bool,
) -> Result<Value, ChatError> {
    if base_url.is_empty() {
        return Err(ChatError::plain("no address configured"));
    }
    let mut req = reqwest::Client::new().post(format!(
        "{}/chat/completions",
        base_url.trim_end_matches('/')
    ));
    if let Some(k) = key {
        req = req.bearer_auth(k);
    }
    let res = req
        .json(body)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| ChatError::plain(format!("could not reach it: {e}")))?;

    let status = res.status();
    let raw = res.text().await.unwrap_or_default();
    if !status.is_success() {
        // The two that are worth naming, because the fix is different and
        // "it returned 401" tells nobody which one it was.
        let message = match status.as_u16() {
            401 | 403 => {
                "it refused the key. Check the key, and that it is for this service.".to_string()
            }
            429 => "it is rate limiting or out of credit.".to_string(),
            _ => format!("it returned {status}"),
        };
        if wants_tools && rejected_the_tools(&raw) {
            return Err(ChatError {
                message: server_complaint(&raw).unwrap_or(message),
                no_tool_support: true,
            });
        }
        return Err(ChatError::plain(message));
    }
    let v: Value = serde_json::from_str(&raw)
        .map_err(|_| ChatError::plain("it returned something unreadable"))?;
    // Some services put a refusal in a 200. Better to say so than to return an
    // empty answer that reads as a model having nothing to add.
    if let Some(msg) = v["error"]["message"].as_str() {
        return Err(ChatError {
            message: msg.to_string(),
            no_tool_support: wants_tools && rejected_the_tools(msg),
        });
    }
    Ok(v)
}

/// Does this refusal say the server or model will not do tool calling?
///
/// Pattern matching on prose, which is unlovely, but there is no field for it:
/// servers say it in words and every one says it differently. Wrong either way
/// is survivable, because the caller's next move is the same sort of honest
/// failure; getting it right just makes the sentence a person reads useful.
fn rejected_the_tools(body: &str) -> bool {
    let b = body.to_lowercase();
    if !b.contains("tool") && !b.contains("function") {
        return false;
    }
    [
        "not supported",
        "unsupported",
        "does not support",
        "doesn't support",
        "no support",
        "unknown parameter",
        "unrecognized",
        "unrecognised",
        "invalid parameter",
        "not allowed",
        "cannot use",
    ]
    .iter()
    .any(|s| b.contains(s))
}

/// The server's own words about what went wrong, if it gave any.
fn server_complaint(raw: &str) -> Option<String> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let msg = v["error"]["message"]
        .as_str()
        .or_else(|| v["error"].as_str())
        .or_else(|| v["message"].as_str())?;
    Some(msg.trim().chars().take(300).collect())
}

/// Read one assistant message, in whichever shape the server used.
///
/// Two shapes are real and both are common: `function.arguments` as a JSON
/// string, which is what the format says, and arguments already parsed into an
/// object, which several local servers do. A model replying with prose and no
/// tool call at all is also normal and is not an error here.
fn read_turn(msg: &Value) -> Turn {
    let text = msg["content"].as_str().unwrap_or_default().to_string();
    let mut tool_calls = vec![];
    for (i, c) in msg["tool_calls"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        let name = c["function"]["name"]
            .as_str()
            .or_else(|| c["name"].as_str())
            .unwrap_or_default()
            .to_string();
        if name.is_empty() {
            continue;
        }
        // Some servers leave the id out entirely, and a tool reply has to quote
        // one, so make a stable one rather than dropping the call.
        let id = c["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("call_{i}"));

        let raw = &c["function"]["arguments"];
        let (arguments, unreadable) = match raw {
            Value::String(s) if s.trim().is_empty() => (json!({}), None),
            Value::String(s) => match serde_json::from_str::<Value>(s) {
                Ok(v) => (v, None),
                Err(_) => (json!({}), Some(s.clone())),
            },
            Value::Object(_) => (raw.clone(), None),
            Value::Null => (json!({}), None),
            other => (json!({}), Some(other.to_string())),
        };
        tool_calls.push(ToolCall {
            id,
            name,
            arguments,
            unreadable,
        });
    }
    Turn { text, tool_calls }
}

/// Anthropic's API, with a key from the keychain.
///
/// The key is read at the moment of use and never stored anywhere else, like
/// every other secret in this program.
async fn ask_anthropic(model: &str, prompt: &str) -> Result<String, String> {
    let key = crate::secrets::get_internal("anthropic.api_key")
        .await
        .map_err(|_| "no Anthropic key is saved. Add one in Settings.".to_string())?;

    let res = reqwest::Client::new()
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", key.expose())
        .header("anthropic-version", "2023-06-01")
        .json(&serde_json::json!({
            "model": model,
            "max_tokens": 1024,
            "messages": [{ "role": "user", "content": prompt }],
        }))
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| format!("could not reach Anthropic: {e}"))?;

    let status = res.status();
    let body = res.text().await.unwrap_or_default();
    if status.as_u16() == 401 {
        return Err("Anthropic refused that key.".into());
    }
    if !status.is_success() {
        return Err(format!("Anthropic returned {status}"));
    }
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|_| "unreadable reply".to_string())?;
    Ok(v["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string())
}

/// Ask an endpoint what it can run.
pub async fn list_models(base_url: &str, key: Option<&str>) -> Result<Vec<String>, String> {
    let mut req = reqwest::Client::new().get(format!("{}/models", base_url.trim_end_matches('/')));
    if let Some(k) = key {
        req = req.bearer_auth(k);
    }
    let res = req
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("{e}"))?;
    if !res.status().is_success() {
        return Err(match res.status().as_u16() {
            401 | 403 => "it refused the key".to_string(),
            other => format!("it returned {other}"),
        });
    }
    let v: serde_json::Value = res.json().await.map_err(|e| format!("{e}"))?;
    Ok(v["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default())
}

/// This provider's key, if one was saved.
///
/// Read at the moment of use and dropped straight afterwards, so a key is never
/// held in memory between runs and never reaches the database or a log.
pub async fn key_for(provider_id: &str) -> Option<String> {
    crate::secrets::get_internal(&errand_core::providers::key_account(provider_id))
        .await
        .ok()
        .map(|s| s.expose().to_string())
}

/// The Claude command line tool, which Errand sets up for itself.
pub const BUILTIN_CLAUDE: &str = "builtin-claude-cli";

/// Make sure there is always something in the list.
///
/// A brand new install has an empty Settings screen otherwise, which reads as
/// "this is broken" rather than "this uses Claude by default". The row is added
/// once and then belongs to the person: switching it off keeps it off.
pub async fn ensure_builtin(pool: &errand_core::db::Pool) -> anyhow::Result<()> {
    let existing = errand_core::db::list_providers(pool).await?;
    if existing.iter().any(|p| p.id == BUILTIN_CLAUDE) {
        return Ok(());
    }
    errand_core::db::upsert_provider(
        pool,
        &Provider {
            id: BUILTIN_CLAUDE.into(),
            kind: Kind::ClaudeCli.as_str().into(),
            label: "Claude (command line tool)".into(),
            base_url: None,
            model: None,
            enabled: true,
            discovered: true,
            health: None,
            health_detail: None,
        },
    )
    .await?;
    Ok(())
}

/// Check each provider and write down what was found, so the Settings screen
/// can say "not answering" instead of showing a hopeful green dot.
pub async fn refresh_health(pool: &errand_core::db::Pool) -> anyhow::Result<()> {
    for p in errand_core::db::list_providers(pool).await? {
        let (status, detail) = check_one(&p).await;
        errand_core::db::set_provider_health(pool, &p.id, status, Some(&detail)).await?;
    }
    Ok(())
}

/// Is this provider actually usable right now?
pub async fn check_one(p: &Provider) -> (&'static str, String) {
    match p.kind_enum() {
        Some(Kind::ClaudeCli) => match crate::executor::find_claude() {
            Some(path) => ("ok", format!("found at {}", path.display())),
            None => (
                "missing",
                "Not installed, or not on the path this background service can see. Install the \
                 Claude command line tool and run 'claude /login' once."
                    .into(),
            ),
        },
        Some(Kind::AnthropicApi) => match crate::secrets::get_internal("anthropic.api_key").await {
            Ok(_) => ("ok", "a key is saved in your keychain".into()),
            Err(_) => ("missing", "No key saved yet.".into()),
        },
        Some(Kind::OpenAiCompat) => {
            let base = p.base_url.clone().unwrap_or_default();
            let key = key_for(&p.id).await;
            if key.is_none() && !p.is_local() {
                return (
                    "missing",
                    "This service needs a key, and none is saved. Add one and check again.".into(),
                );
            }
            match list_models(&base, key.as_deref()).await {
                Ok(m) if m.is_empty() => (
                    "empty",
                    "It answered, but has no models loaded. Load one and check again.".into(),
                ),
                Ok(m) => ("ok", format!("{} model(s): {}", m.len(), m.join(", "))),
                Err(e) => ("unreachable", format!("Nothing answered at {base}. {e}")),
            }
        }
        None => ("unknown", "Errand does not recognise this kind.".into()),
    }
}

/// What a scan turned up, including the things it could not use.
///
/// The near misses matter as much as the hits. A scan that quietly drops an
/// endpoint needing a key, or one with no model loaded, looks identical to a
/// network with nothing on it, and the person is left believing Errand looked
/// properly when it looked and shrugged.
#[derive(Debug, Default)]
pub struct Discovery {
    pub found: Vec<Provider>,
    /// Answered, but not usable as it stands. Reported with the reason.
    pub also_seen: Vec<(String, String)>,
    pub addresses: usize,
    pub ports: usize,
    /// Set when macOS is refusing this process the local network entirely, in
    /// which case an empty result says nothing about what is out there.
    pub blocked: Option<String>,
}

/// Is macOS refusing this process access to the local network?
///
/// Since Sequoia, a program needs the user's permission to talk to anything on
/// the local network, and a background service started by launchd has no way to
/// raise that prompt where anybody would see it. Denied connections then fail
/// instantly, so a sweep finishes in half a second and reports nothing, which
/// is indistinguishable from a network with nothing on it, and sends people off
/// to debug their model server instead of their privacy settings.
///
/// The router is the test. Every working LAN has one, it answers on at least
/// one of these ports, and it is the one address on the network that can be
/// found without guessing.
async fn local_network_blocked() -> bool {
    let Some(gateway) = default_gateway() else {
        // No gateway means no LAN to be blocked from: Wi-Fi off, or a machine
        // with only loopback. Not this problem.
        return false;
    };
    for port in [80u16, 443, 53, 8080] {
        let ok = tokio::time::timeout(
            std::time::Duration::from_millis(700),
            tokio::net::TcpStream::connect(format!("{gateway}:{port}")),
        )
        .await;
        if matches!(ok, Ok(Ok(_))) {
            return false;
        }
    }
    true
}

/// How many sockets this process may safely have open at once.
///
/// Two thirds of whatever is left after the descriptors already in use, capped
/// at a number that is polite to the network stack. The daemon also raises its
/// own soft limit at boot, but this has to stand on its own: a limit that was
/// not raised must slow the scan down, never silently empty it.
fn socket_budget() -> usize {
    let limit = file_descriptor_limit().unwrap_or(256);
    // Leave room for the database pool, the log, the browser and the API.
    let spare = limit.saturating_sub(64);
    (spare * 2 / 3).clamp(16, 256)
}

/// The soft limit on open files, or None if it cannot be read.
fn file_descriptor_limit() -> Option<usize> {
    // SAFETY: getrlimit fills a struct we own and returns 0 on success.
    unsafe {
        let mut rl: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) != 0 {
            return None;
        }
        Some(rl.rlim_cur as usize)
    }
}

/// Ask for as many open files as the system will allow.
///
/// Called once at boot. launchd's default of 256 is enough for ordinary work
/// and far too few for a network sweep, and raising the soft limit to the hard
/// one needs no privileges; it is what every server does.
pub fn raise_file_descriptor_limit() {
    // SAFETY: both calls operate on a struct we own.
    unsafe {
        let mut rl: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) != 0 {
            return;
        }
        let wanted = rl.rlim_max.min(8192);
        if rl.rlim_cur >= wanted {
            return;
        }
        let was = rl.rlim_cur;
        rl.rlim_cur = wanted;
        if libc::setrlimit(libc::RLIMIT_NOFILE, &rl) == 0 {
            tracing::debug!("raised the open-file limit from {was} to {wanted}");
        }
    }
}

fn default_gateway() -> Option<String> {
    let out = std::process::Command::new("/sbin/route")
        .args(["-n", "get", "default"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("gateway:")
                .map(|g| g.trim().to_string())
        })
        .filter(|g| g.parse::<std::net::Ipv4Addr>().is_ok())
}

/// What to tell somebody whose scan was refused by the operating system.
pub const LOCAL_NETWORK_BLOCKED: &str =
    "macOS is not letting Errand's background service reach your local network, so it could only \
     look at this machine. Open System Settings, go to Privacy & Security, then Local Network, and \
     switch on Errand-AI. Then try again. Until that is done, a model on another machine cannot be \
     found or used, however it is added.";

/// Look for models, on this machine and optionally on this network.
///
/// Two quite different things, which is why they are separate. Loopback is
/// always safe to try: it is your own computer and nobody else can see it.
/// Scanning the network is not something to do behind someone's back: on an
/// office or hotel network it is rude, and it looks like something worse, so
/// it happens only when asked for, and only across the subnet this machine is
/// already on.
///
/// Nothing found is switched on. Finding a model and trusting it are separate
/// decisions, and the second one is not Errand's to make.
///
/// What this CANNOT find, and no address sweep could: anything reached by name
/// rather than by number. A server behind a reverse proxy that routes on the
/// hostname (an Olares app, a Tailscale name, anything with its own domain)
/// answers nothing useful at its bare address. Those have to be added by
/// address, which is why that option sits next to this one.
pub async fn discover(scan_network: bool) -> Discovery {
    let mut hosts: Vec<String> = vec!["127.0.0.1".into()];
    if scan_network {
        hosts.extend(subnet_hosts());
    }

    // Every port, everywhere. An earlier version tried only five of them across
    // a network, on the theory that the long tail was other people's web
    // servers, but the cost of a closed port is one refused connection, and
    // the cost of missing somebody's model server is that they conclude the
    // feature does not work.
    let mut jobs = vec![];
    for host in &hosts {
        for probe in errand_core::providers::PROBES {
            jobs.push((host.clone(), probe.port, probe.what));
        }
    }

    tracing::info!(
        addresses = hosts.len(),
        ports = errand_core::providers::PROBES.len(),
        probes = jobs.len(),
        scan_network,
        "looking for models"
    );

    // How many sockets to have open at once.
    //
    // Not a constant, because the honest answer depends on where this process
    // is running. launchd hands its children a soft limit of 256 open files
    // while a terminal gets a million, so a sweep tuned for a terminal opens
    // every socket it is allowed and then fails instantly on all the rest,
    // giving a scan that finishes in half a second and finds nothing at all,
    // which reads exactly like a network with nothing on it. That is precisely
    // the bug this comment exists to stop somebody reintroducing.
    let limit = std::sync::Arc::new(tokio::sync::Semaphore::new(socket_budget()));
    let mut tasks = tokio::task::JoinSet::new();
    for (host, port, what) in jobs {
        let limit = limit.clone();
        tasks.spawn(async move {
            let _permit = limit.acquire_owned().await.ok()?;

            // A plain connection first. Almost everything scanned is nothing at
            // all, and a refused connection costs a millisecond where an HTTP
            // request to a dead address costs the whole timeout.
            let addr = format!("{host}:{port}");
            let connected = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                tokio::net::TcpStream::connect(&addr),
            )
            .await;
            if !matches!(connected, Ok(Ok(_))) {
                return None;
            }
            tracing::debug!(%addr, "something is listening");

            let scheme = if errand_core::providers::TLS_PORTS.contains(&port) {
                "https"
            } else {
                "http"
            };
            let base = format!("{scheme}://{host}:{port}/v1");
            let where_ = if host == "127.0.0.1" {
                format!("{what} on this machine")
            } else {
                format!("{what} on {host}")
            };

            let outcome =
                tokio::time::timeout(std::time::Duration::from_secs(4), list_models(&base, None))
                    .await;

            let models = match outcome {
                // Something is listening but never answered. Almost always a
                // service that is not HTTP at all, so not worth reporting.
                Err(_) => {
                    tracing::debug!(%addr, "listening, but did not answer");
                    return None;
                }
                Ok(Ok(m)) if m.is_empty() => {
                    return Some(Err((
                        base,
                        format!("{where_} answered, but has no model loaded"),
                    )));
                }
                Ok(Ok(m)) => m,
                Ok(Err(e)) => {
                    tracing::debug!(%addr, "listening, but not a model server: {e}");
                    // A refused key is the interesting one: it means an
                    // OpenAI-compatible server really is there and only needs
                    // adding by hand with its key.
                    if e.contains("refused the key") {
                        return Some(Err((
                            base,
                            format!("{where_} needs a key. Add it by address below."),
                        )));
                    }
                    return None;
                }
            };

            Some(Ok(Provider {
                id: format!("found:{base}"),
                kind: Kind::OpenAiCompat.as_str().into(),
                label: where_,
                base_url: Some(base),
                model: models.first().cloned(),
                enabled: false,
                discovered: true,
                health: Some("ok".into()),
                health_detail: Some(format!(
                    "{} model(s): {}",
                    models.len(),
                    models
                        .iter()
                        .take(4)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            }))
        });
    }

    let mut out = Discovery {
        addresses: hosts.len(),
        ports: errand_core::providers::PROBES.len(),
        ..Default::default()
    };
    // Asked only when it matters, and only after the sweep, so the check costs
    // nothing on the common path.
    if scan_network && local_network_blocked().await {
        out.blocked = Some(LOCAL_NETWORK_BLOCKED.to_string());
    }
    while let Some(res) = tasks.join_next().await {
        match res {
            Ok(Some(Ok(p))) => out.found.push(p),
            Ok(Some(Err((url, why)))) => out.also_seen.push((url, why)),
            _ => {}
        }
    }
    // A stable order, so the same scan twice does not reshuffle the list.
    out.found.sort_by(|a, b| a.base_url.cmp(&b.base_url));
    out.also_seen.sort();
    out
}

/// Every other address on the networks this machine is already on.
///
/// Only a /24, and only for interfaces that actually have a private address.
/// A machine on a large corporate network is not going to be swept, both
/// because it would take all day and because it is not Errand's business.
fn subnet_hosts() -> Vec<String> {
    let mut hosts = vec![];
    for ip in own_addresses() {
        let o = ip.octets();
        if !ip.is_private() {
            continue;
        }
        for last in 1u16..=254 {
            let candidate = format!("{}.{}.{}.{}", o[0], o[1], o[2], last);
            // Skip ourselves: loopback covers this machine already.
            if last as u8 != o[3] {
                hosts.push(candidate);
            }
        }
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

/// This machine's own IPv4 addresses.
fn own_addresses() -> Vec<std::net::Ipv4Addr> {
    let Ok(out) = std::process::Command::new("/sbin/ifconfig")
        .arg("-a")
        .output()
    else {
        return vec![];
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut found = vec![];
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("inet ") else {
            continue;
        };
        let Some(addr) = rest.split_whitespace().next() else {
            continue;
        };
        if let Ok(ip) = addr.parse::<std::net::Ipv4Addr>() {
            if ip.is_private() {
                found.push(ip);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_network_is_never_scanned_unless_it_was_asked_for() {
        // Sweeping somebody's network unasked is rude, and on a work network it
        // looks like something else entirely. The default must stay loopback.
        let d = discover(false).await;
        assert_eq!(
            d.addresses, 1,
            "only this machine should have been looked at"
        );
        for url in d
            .found
            .iter()
            .filter_map(|p| p.base_url.clone())
            .chain(d.also_seen.iter().map(|(u, _)| u.clone()))
        {
            assert!(
                url.starts_with("http://127.0.0.1:"),
                "an unasked-for scan reached {url}"
            );
        }
    }

    #[tokio::test]
    async fn a_scan_says_how_hard_it_looked_even_when_it_finds_nothing() {
        // An empty list has two very different meanings, "there is nothing
        // there" and "it did not really look", and only one of them is worth
        // acting on. The counts are what tell them apart.
        let d = discover(false).await;
        assert_eq!(d.ports, errand_core::providers::PROBES.len());
        assert!(
            d.ports >= 15,
            "a handful of ports misses most of the ecosystem"
        );
    }

    #[test]
    fn a_sweep_covers_the_subnet_and_leaves_out_this_machine() {
        let hosts = subnet_hosts();
        for h in &hosts {
            let ip: std::net::Ipv4Addr = h.parse().expect("every candidate is an address");
            assert!(
                ip.is_private(),
                "a sweep must stay on a private network: {h}"
            );
        }
        // Either this machine has no private address, or the sweep is a /24
        // with this machine itself left out.
        if !hosts.is_empty() {
            assert!(
                hosts.len().is_multiple_of(253),
                "a sweep is a /24 minus ourselves"
            );
            for own in own_addresses() {
                assert!(
                    !hosts.contains(&own.to_string()),
                    "the sweep should not include this machine"
                );
            }
        }
    }

    #[tokio::test]
    async fn an_endpoint_that_is_not_there_fails_quickly_and_clearly() {
        // Port 9 discards everything, so nothing will answer.
        let e = ask_openai_compatible("http://127.0.0.1:9", "any", "hello", None)
            .await
            .unwrap_err();
        assert!(e.contains("could not reach it"), "unhelpful: {e}");
    }

    #[tokio::test]
    async fn an_empty_address_is_refused_rather_than_requested() {
        assert!(ask_openai_compatible("", "m", "p", None).await.is_err());
    }

    /// A stand-in for Ollama. Raw TCP rather than a web framework, because the
    /// point is to prove Errand speaks the wire format, not to test a library.
    async fn stub(reply: &'static str) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = l.accept().await else {
                    return;
                };
                let body = reply.to_string();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let res = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(res.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn a_local_model_really_answers() {
        let base =
            stub(r#"{"choices":[{"message":{"content":"CAUSE: the site was down"}}]}"#).await;
        let got = ask_openai_compatible(&base, "llama3.1", "why did it fail?", None)
            .await
            .unwrap();
        assert_eq!(got, "CAUSE: the site was down");
    }

    #[tokio::test]
    async fn a_reply_in_a_shape_errand_does_not_know_is_an_error_not_a_shrug() {
        // An endpoint answering 200 with something else must not be read as an
        // empty answer, or a run would act on nothing and call it a diagnosis.
        let base = stub(r#"{"unexpected":true}"#).await;
        let got = ask_openai_compatible(&base, "m", "p", None).await.unwrap();
        assert!(
            got.is_empty(),
            "unknown shapes yield nothing, and ask() treats that as a failure"
        );
    }

    #[tokio::test]
    async fn when_the_first_choice_is_dead_the_question_is_still_answered() {
        // The whole reason for a chain. Somebody points the Fixer at the model
        // on their desk, the machine is asleep, and a run must not die of it.
        let pool = errand_core::db::open_memory().await.unwrap();
        let alive = stub(r#"{"choices":[{"message":{"content":"the other one answered"}}]}"#).await;

        for (id, label, url) in [
            ("dead", "Desk machine", "http://127.0.0.1:9"),
            ("alive", "Spare", alive.as_str()),
        ] {
            errand_core::db::upsert_provider(
                &pool,
                &Provider {
                    id: id.into(),
                    kind: Kind::OpenAiCompat.as_str().into(),
                    label: label.into(),
                    base_url: Some(url.into()),
                    model: Some("m".into()),
                    enabled: true,
                    discovered: false,
                    health: None,
                    health_detail: None,
                },
            )
            .await
            .unwrap();
        }
        errand_core::db::set_role_binding(&pool, Role::Fixer, Some("dead"))
            .await
            .unwrap();

        let state = AppState::new(pool);
        let answer = ask(&state, Role::Fixer, "why?").await.unwrap();

        assert_eq!(answer.text, "the other one answered");
        assert_eq!(answer.provider_label, "Spare");
        assert!(answer.was_local);
    }

    #[tokio::test]
    async fn with_nowhere_to_send_it_the_error_says_what_to_do() {
        let pool = errand_core::db::open_memory().await.unwrap();
        let state = AppState::new(pool);
        let e = ask(&state, Role::Narrator, "hello")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("claude /login"),
            "an empty setup should say how to fix it, got: {e}"
        );
    }

    // ------------------------------------------------------ calling tools --

    fn one_call(arguments: Value) -> Value {
        json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{ "id": "abc", "type": "function",
                             "function": { "name": "journal", "arguments": arguments } }]
        })
    }

    #[test]
    fn arguments_are_understood_whether_they_arrive_as_text_or_as_an_object() {
        // Both shapes are out there. Reading only the one the specification
        // describes would drop every call from several local servers, which is
        // exactly the sort of thing that reads as "local models do not work".
        for arguments in [
            json!("{\"title\":\"Opened the basket\"}"),
            json!({ "title": "Opened the basket" }),
        ] {
            let turn = read_turn(&one_call(arguments.clone()));
            assert_eq!(turn.tool_calls.len(), 1, "dropped a call: {arguments}");
            let call = &turn.tool_calls[0];
            assert_eq!(call.name, "journal");
            assert_eq!(call.arguments["title"], "Opened the basket");
            assert!(call.unreadable.is_none());
        }
    }

    #[test]
    fn arguments_that_are_not_readable_are_reported_rather_than_guessed_at() {
        // Dispatching an empty object here would be worse than useless: the
        // tool would do something other than what was asked for.
        let turn = read_turn(&one_call(json!("title: Opened the basket")));
        let call = &turn.tool_calls[0];
        assert_eq!(
            call.unreadable.as_deref(),
            Some("title: Opened the basket"),
            "unreadable arguments must be kept, so the model can be told what it sent"
        );
    }

    #[test]
    fn a_call_with_no_id_still_gets_a_conversation_that_hangs_together() {
        // Several servers leave the id out. A tool reply has to quote one, so
        // dropping the call or inventing a different id each time would break
        // the very next turn.
        let msg = json!({
            "role": "assistant",
            "tool_calls": [{ "function": { "name": "finish", "arguments": "{}" } }]
        });
        let turn = read_turn(&msg);
        assert_eq!(turn.tool_calls.len(), 1);
        let id = turn.tool_calls[0].id.clone();
        assert!(!id.is_empty());
        assert_eq!(
            turn.as_message()["tool_calls"][0]["id"].as_str(),
            Some(id.as_str()),
            "the id the tool reply will quote must be the one in the assistant message"
        );
    }

    #[test]
    fn a_model_answering_in_words_is_ordinary_rather_than_broken() {
        let turn = read_turn(&json!({ "role": "assistant", "content": "I would start by..." }));
        assert!(turn.tool_calls.is_empty());
        assert!(turn.text.starts_with("I would start"));
    }

    #[test]
    fn a_server_that_will_not_do_tools_is_told_apart_from_a_bad_key() {
        // The fix is different: one means pick another model, the other means
        // check the key. Saying the wrong one costs somebody an evening.
        for refusal in [
            "this model does not support tools",
            "tool_choice is not supported",
            "Unknown parameter: 'tools'",
            "function calling unsupported for this model",
        ] {
            assert!(rejected_the_tools(refusal), "missed: {refusal}");
        }
        for other in [
            "invalid api key",
            "rate limit exceeded",
            "model not found",
            "context length exceeded",
        ] {
            assert!(!rejected_the_tools(other), "wrongly blamed tools: {other}");
        }
    }

    #[tokio::test]
    async fn the_model_chosen_for_the_task_is_the_one_asked_to_do_it() {
        // The point of the whole change: a model on your own machine can be the
        // one that carries out the task, and being chosen means being first.
        let pool = errand_core::db::open_memory().await.unwrap();
        for (id, kind, url, enabled) in [
            (BUILTIN_CLAUDE, Kind::ClaudeCli, None, true),
            (
                "desk",
                Kind::OpenAiCompat,
                Some("http://127.0.0.1:11434/v1"),
                true,
            ),
            (
                "asleep",
                Kind::OpenAiCompat,
                Some("http://127.0.0.1:1234/v1"),
                false,
            ),
        ] {
            errand_core::db::upsert_provider(
                &pool,
                &Provider {
                    id: id.into(),
                    kind: kind.as_str().into(),
                    label: id.into(),
                    base_url: url.map(str::to_string),
                    model: Some("qwen3.5-27b".into()),
                    enabled,
                    discovered: false,
                    health: None,
                    health_detail: None,
                },
            )
            .await
            .unwrap();
        }
        errand_core::db::set_role_binding(&pool, Role::Executor, Some("desk"))
            .await
            .unwrap();

        let state = AppState::new(pool);
        let chain = executor_chain(&state).await.expect("something can do it");

        assert_eq!(
            chain.first().map(|p| p.id.as_str()),
            Some("desk"),
            "the model the person picked has to be the one that gets the job"
        );
        assert!(
            chain.iter().any(|p| p.id == BUILTIN_CLAUDE),
            "the others stay behind it, so a machine that is asleep does not stop the run"
        );
        assert!(
            !chain.iter().any(|p| p.id == "asleep"),
            "a model that is switched off must not be asked to do anything"
        );
    }

    #[tokio::test]
    async fn a_task_kept_on_this_machine_is_never_handed_to_a_service() {
        let pool = errand_core::db::open_memory().await.unwrap();
        errand_core::db::upsert_provider(
            &pool,
            &Provider {
                id: "a service".into(),
                kind: Kind::OpenAiCompat.as_str().into(),
                label: "Somebody else's computer".into(),
                base_url: Some("https://api.example.com/v1".into()),
                model: Some("big".into()),
                enabled: true,
                discovered: false,
                health: None,
                health_detail: None,
            },
        )
        .await
        .unwrap();
        errand_core::db::set_setting(&pool, "privacy.local_only", &json!(true))
            .await
            .unwrap();

        let state = AppState::new(pool);
        let e = executor_chain(&state)
            .await
            .expect_err("nothing local is switched on")
            .to_string();
        assert!(
            e.contains("stay on your own machine"),
            "it must say why it refused, not just that it did: {e}"
        );
    }
}

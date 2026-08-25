//! Where the thinking happens.
//!
//! Errand does not ship with an AI. It uses whichever ones you already have,
//! and it is deliberately honest about what each can be trusted with.
//!
//! There are two quite different jobs here, and conflating them is how a
//! product promises "use any model" and then quietly does not:
//!
//! **Carrying out a task** means driving a browser over many turns, calling
//! tools, and deciding what to do next. That needs a model with a working
//! agent loop and tool use. Today that is the Claude command line tool, which
//! brings its own loop and its own containment.
//!
//! **Everything else** is a single question with a single answer: diagnose this
//! failure, summarise this run, distil these steps. Any competent model can do
//! that, including one running on your own machine, and there is no reason to
//! send it to a cloud.
//!
//! So a local model is genuinely useful here, and this module says exactly
//! which roles it can fill rather than implying it can fill all of them.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

/// What sort of thing this endpoint is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The Claude command line tool, using your existing Claude login.
    ClaudeCli,
    /// Anthropic's API directly, with a key you supply.
    AnthropicApi,
    /// Anything speaking the OpenAI chat format: Ollama, LM Studio, vLLM,
    /// llama.cpp, Open WebUI, or a hosted service.
    OpenAiCompat,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ClaudeCli => "claude_cli",
            Self::AnthropicApi => "anthropic_api",
            Self::OpenAiCompat => "openai_compat",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "claude_cli" => Self::ClaudeCli,
            "anthropic_api" => Self::AnthropicApi,
            "openai_compat" => Self::OpenAiCompat,
            _ => return None,
        })
    }

    /// Can this run a whole task, driving a browser over many turns?
    ///
    /// Only the Claude command line tool, for now. Making a plain chat endpoint
    /// do this means writing the agent loop, the tool protocol and the
    /// containment again, and claiming it works before that exists would be a
    /// lie a user only discovers at 08:00 on a Wednesday.
    pub fn can_carry_out_tasks(&self) -> bool {
        matches!(self, Self::ClaudeCli)
    }

    /// Does anything leave your machine when this is used?
    pub fn is_local(&self, base_url: Option<&str>) -> bool {
        match self {
            Self::ClaudeCli | Self::AnthropicApi => false,
            Self::OpenAiCompat => base_url.map(is_local_url).unwrap_or(false),
        }
    }
}

/// Loopback, a private network, or a .local name.
pub fn is_local_url(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    let Some(host) = u.host_str() else {
        return false;
    };
    if host == "localhost" || host.ends_with(".local") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => ip.is_loopback() || ip.is_private(),
        Ok(std::net::IpAddr::V6(ip)) => ip.is_loopback(),
        Err(_) => false,
    }
}

/// The four jobs a model can be given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Carries out the task. Needs an agentic provider.
    Executor,
    /// Turns a finished run into a written plan.
    Planner,
    /// Works out why something failed and what to try instead.
    Fixer,
    /// Writes the summary you read and the messages you receive.
    Narrator,
}

impl Role {
    pub const ALL: [Role; 4] = [Role::Executor, Role::Planner, Role::Fixer, Role::Narrator];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Executor => "executor",
            Self::Planner => "planner",
            Self::Fixer => "fixer",
            Self::Narrator => "narrator",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "executor" => Self::Executor,
            "planner" => Self::Planner,
            "fixer" => Self::Fixer,
            "narrator" => Self::Narrator,
            _ => return None,
        })
    }

    /// Does this role have to drive a browser?
    pub fn needs_agentic(&self) -> bool {
        matches!(self, Self::Executor)
    }

    /// Is this role actually consulted yet?
    ///
    /// This exists so the interface cannot offer a choice that does nothing.
    /// The Planner is not wired: the plan is written by the agent itself, at
    /// the end of a run, because it is the only thing that watched the run
    /// happen. Until that is separated out, offering a model for it would be a
    /// setting people change and then wonder why nothing differs.
    pub fn is_wired(&self) -> bool {
        true
    }

    /// Why a role is not in use, for the interface to say out loud.
    ///
    /// Every role is consulted now. Kept, rather than deleted, because the rule
    /// it enforces is the valuable part: a role that stops being used owes the
    /// interface a reason, instead of leaving a setting that changes nothing.
    pub fn not_wired_reason(&self) -> Option<&'static str> {
        None
    }

    /// Said in the interface, so nobody has to guess what a role is for.
    pub fn describe(&self) -> &'static str {
        match self {
            Self::Executor => {
                "Actually does the task: opens the browser, signs in, decides what to click. \
                 Needs a model that can use tools over many turns, so this must be Claude for now."
            }
            Self::Planner => {
                "Writes the plan you approve when a run finishes without leaving one. Normally the \
                 agent writes its own, because it is the only thing that watched the run; this is \
                 the fallback, worked out from the record of what happened."
            }
            Self::Fixer => {
                "Reads a failed run and suggests what to try instead. One question, one answer."
            }
            Self::Narrator => {
                "Writes the summaries you read and the messages you get sent. Worth keeping local \
                 if you would rather your run history stayed on your own machine."
            }
        }
    }
}

/// One place Errand can send a question.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    pub kind: String,
    pub label: String,
    /// For OpenAI-compatible endpoints. None for the others.
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub enabled: bool,
    /// True when this was found by looking rather than typed in.
    pub discovered: bool,
    pub health: Option<String>,
    pub health_detail: Option<String>,
}

impl Provider {
    pub fn kind_enum(&self) -> Option<Kind> {
        Kind::parse(&self.kind)
    }
    pub fn is_local(&self) -> bool {
        self.kind_enum()
            .map(|k| k.is_local(self.base_url.as_deref()))
            .unwrap_or(false)
    }
    pub fn can_carry_out_tasks(&self) -> bool {
        self.kind_enum()
            .map(|k| k.can_carry_out_tasks())
            .unwrap_or(false)
    }

    /// Why this provider cannot fill this role, if it cannot.
    pub fn cannot_fill(&self, role: Role) -> Option<String> {
        if role.needs_agentic() && !self.can_carry_out_tasks() {
            return Some(format!(
                "{} answers one question at a time. Carrying out a task means driving a browser \
                 over many turns and calling tools, which needs the Claude command line tool. It \
                 can do any of the other three jobs.",
                self.label
            ));
        }
        if !self.enabled {
            return Some(format!("{} is switched off.", self.label));
        }
        None
    }
}

/// A service Errand already knows how to talk to.
///
/// The point of this list is that nobody should have to know a base URL. You
/// pick the name you recognise, paste a key, and it works. Everything here
/// speaks the OpenAI chat format, which is the one thing the industry agreed
/// on, so a single adapter reaches all of them.
#[derive(Debug, Clone, Serialize)]
pub struct Known {
    pub id: &'static str,
    pub name: &'static str,
    /// The OpenAI-compatible root, including the version segment. Exact, so
    /// nothing has to be guessed at request time.
    pub base_url: &'static str,
    /// What a key from this service starts with. Empty when they have no fixed
    /// prefix, in which case Errand does not pretend to validate it.
    pub key_prefix: &'static str,
    /// Where to get a key. Shown next to the box you paste it into.
    pub keys_url: &'static str,
    /// A model that exists there, so the field is not an empty guess.
    pub example_model: &'static str,
    pub needs_key: bool,
}

/// The services worth offering by name.
///
/// Ordered roughly by how likely someone is to be looking for it. Anthropic is
/// absent on purpose: it has its own entry, because it does not speak this
/// format natively and Errand talks to it properly instead.
pub const CATALOGUE: &[Known] = &[
    Known {
        id: "openai",
        name: "OpenAI",
        base_url: "https://api.openai.com/v1",
        key_prefix: "sk-",
        keys_url: "https://platform.openai.com/api-keys",
        example_model: "gpt-4o-mini",
        needs_key: true,
    },
    Known {
        id: "google",
        name: "Google Gemini",
        // Google publishes an OpenAI-shaped endpoint alongside its own, which
        // is the one worth using here: one adapter, one code path.
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        key_prefix: "",
        keys_url: "https://aistudio.google.com/apikey",
        example_model: "gemini-2.0-flash",
        needs_key: true,
    },
    Known {
        id: "openrouter",
        name: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        key_prefix: "sk-or-",
        keys_url: "https://openrouter.ai/keys",
        example_model: "openai/gpt-4o-mini",
        needs_key: true,
    },
    Known {
        id: "xai",
        name: "xAI (Grok)",
        base_url: "https://api.x.ai/v1",
        key_prefix: "xai-",
        keys_url: "https://console.x.ai",
        example_model: "grok-2-latest",
        needs_key: true,
    },
    Known {
        id: "deepseek",
        name: "DeepSeek",
        base_url: "https://api.deepseek.com/v1",
        key_prefix: "sk-",
        keys_url: "https://platform.deepseek.com/api_keys",
        example_model: "deepseek-chat",
        needs_key: true,
    },
    Known {
        id: "moonshot",
        name: "Moonshot (Kimi)",
        base_url: "https://api.moonshot.ai/v1",
        key_prefix: "sk-",
        keys_url: "https://platform.moonshot.ai/console/api-keys",
        example_model: "kimi-k2-0905-preview",
        needs_key: true,
    },
    Known {
        id: "mistral",
        name: "Mistral",
        base_url: "https://api.mistral.ai/v1",
        key_prefix: "",
        keys_url: "https://console.mistral.ai/api-keys",
        example_model: "mistral-large-latest",
        needs_key: true,
    },
    Known {
        id: "groq",
        name: "Groq",
        base_url: "https://api.groq.com/openai/v1",
        key_prefix: "gsk_",
        keys_url: "https://console.groq.com/keys",
        example_model: "llama-3.3-70b-versatile",
        needs_key: true,
    },
    Known {
        id: "zai",
        name: "Z.ai (GLM)",
        base_url: "https://api.z.ai/api/paas/v4",
        key_prefix: "",
        keys_url: "https://z.ai/manage-apikey/apikey-list",
        example_model: "glm-4.6",
        needs_key: true,
    },
    Known {
        id: "together",
        name: "Together AI",
        base_url: "https://api.together.xyz/v1",
        key_prefix: "",
        keys_url: "https://api.together.ai/settings/api-keys",
        example_model: "Qwen/Qwen2.5-72B-Instruct-Turbo",
        needs_key: true,
    },
    Known {
        id: "fireworks",
        name: "Fireworks",
        base_url: "https://api.fireworks.ai/inference/v1",
        key_prefix: "",
        keys_url: "https://fireworks.ai/account/api-keys",
        example_model: "accounts/fireworks/models/llama-v3p3-70b-instruct",
        needs_key: true,
    },
    Known {
        id: "cerebras",
        name: "Cerebras",
        base_url: "https://api.cerebras.ai/v1",
        key_prefix: "csk-",
        keys_url: "https://cloud.cerebras.ai",
        example_model: "llama-3.3-70b",
        needs_key: true,
    },
    Known {
        id: "perplexity",
        name: "Perplexity",
        base_url: "https://api.perplexity.ai",
        key_prefix: "pplx-",
        keys_url: "https://www.perplexity.ai/settings/api",
        example_model: "sonar",
        needs_key: true,
    },
    Known {
        id: "ollama",
        name: "Ollama (on this machine)",
        base_url: "http://127.0.0.1:11434/v1",
        key_prefix: "",
        keys_url: "https://ollama.com/download",
        example_model: "llama3.2",
        needs_key: false,
    },
    Known {
        id: "lmstudio",
        name: "LM Studio (on this machine)",
        base_url: "http://127.0.0.1:1234/v1",
        key_prefix: "",
        keys_url: "https://lmstudio.ai",
        example_model: "",
        needs_key: false,
    },
];

pub fn known(id: &str) -> Option<&'static Known> {
    CATALOGUE.iter().find(|k| k.id == id)
}

/// Where a key lives in the keychain. One account per provider, so removing a
/// provider can remove exactly its key and nothing else.
pub fn key_account(provider_id: &str) -> String {
    format!("provider-key-{provider_id}")
}

/// Somewhere worth looking for a model on this machine or network.
pub struct Probe {
    pub port: u16,
    pub what: &'static str,
    /// Path that lists models, appended to the base.
    pub models_path: &'static str,
}

/// The usual homes of a local model.
///
/// Deliberately a list of ports rather than of addresses: shipping somebody
/// else's machine address as a default is how a public build ends up quietly
/// pointing at a stranger's computer.
pub const PROBES: &[Probe] = &[
    Probe {
        port: 11434,
        what: "Ollama",
        models_path: "/v1/models",
    },
    Probe {
        port: 11435,
        what: "Ollama",
        models_path: "/v1/models",
    },
    Probe {
        port: 1234,
        what: "LM Studio",
        models_path: "/v1/models",
    },
    Probe {
        port: 1235,
        what: "LM Studio",
        models_path: "/v1/models",
    },
    Probe {
        port: 8000,
        what: "vLLM",
        models_path: "/v1/models",
    },
    Probe {
        port: 8001,
        what: "vLLM",
        models_path: "/v1/models",
    },
    Probe {
        port: 8080,
        what: "llama.cpp",
        models_path: "/v1/models",
    },
    Probe {
        port: 8081,
        what: "llama.cpp",
        models_path: "/v1/models",
    },
    Probe {
        port: 8090,
        what: "a model server",
        models_path: "/v1/models",
    },
    Probe {
        port: 3000,
        what: "Open WebUI",
        models_path: "/v1/models",
    },
    Probe {
        port: 3001,
        what: "AnythingLLM",
        models_path: "/v1/models",
    },
    Probe {
        port: 4000,
        what: "LiteLLM",
        models_path: "/v1/models",
    },
    Probe {
        port: 4891,
        what: "GPT4All",
        models_path: "/v1/models",
    },
    Probe {
        port: 5000,
        what: "a model server",
        models_path: "/v1/models",
    },
    Probe {
        port: 5001,
        what: "KoboldCpp",
        models_path: "/v1/models",
    },
    Probe {
        port: 1337,
        what: "Jan",
        models_path: "/v1/models",
    },
    Probe {
        port: 9997,
        what: "Xinference",
        models_path: "/v1/models",
    },
    Probe {
        port: 30000,
        what: "SGLang",
        models_path: "/v1/models",
    },
    Probe {
        port: 7860,
        what: "a model server",
        models_path: "/v1/models",
    },
    Probe {
        port: 8443,
        what: "a model server",
        models_path: "/v1/models",
    },
];

/// Ports where a server is normally behind TLS, so the scan speaks https there.
///
/// Everything else is tried over plain http: a model server on a home network
/// almost never has a certificate, and trying both on every port would double a
/// scan that is already the slowest thing in the app.
pub const TLS_PORTS: &[u16] = &[443, 8443];

/// Which model each role uses when nobody has said otherwise.
///
/// Everything defaults to Claude because it is the only thing guaranteed to be
/// there. A local model is an improvement you opt into, not an assumption.
pub fn default_model_for(role: Role) -> &'static str {
    match role {
        // The task itself is worth the better model.
        Role::Executor => "sonnet",
        Role::Planner => "sonnet",
        // A diagnosis and a summary are small jobs.
        Role::Fixer => "haiku",
        Role::Narrator => "haiku",
    }
}

/// Where a role's question should go, given what is configured.
///
/// Returns the chain to try in order. A chain rather than one answer because a
/// local model that is switched off should fall back rather than fail the run.
pub fn resolve_chain<'a>(
    providers: &'a [Provider],
    bindings: &[(Role, String)],
    role: Role,
    local_only: bool,
) -> Vec<&'a Provider> {
    let mut chain: Vec<&Provider> = vec![];

    // What the person chose for this role, first.
    for (r, pid) in bindings {
        if *r != role {
            continue;
        }
        if let Some(p) = providers.iter().find(|p| &p.id == pid) {
            if p.cannot_fill(role).is_none() {
                chain.push(p);
            }
        }
    }

    // Then anything else that could do the job, so one endpoint being down does
    // not stop a run.
    for p in providers {
        if !p.enabled || chain.iter().any(|c| c.id == p.id) {
            continue;
        }
        if p.cannot_fill(role).is_some() {
            continue;
        }
        chain.push(p);
    }

    if local_only {
        chain.retain(|p| p.is_local());
    }
    chain
}

/// Why a role has nowhere to send its question, in words a person can act on.
pub fn explain_empty_chain(role: Role, local_only: bool, providers: &[Provider]) -> String {
    if providers.is_empty() {
        return "Errand has no AI to work with. It uses the Claude command line tool by default; \
                install it and run 'claude /login' once, or add a model of your own in Settings."
            .into();
    }
    if local_only {
        return format!(
            "This task is set to stay on your own machine, and nothing local can do the {} job. \
             Add a local model in Settings, or turn that setting off for this task.",
            role.as_str()
        );
    }
    if role.needs_agentic() {
        return "Carrying out a task needs the Claude command line tool, and Errand cannot find a \
                working one. Install it and run 'claude /login' once."
            .into();
    }
    format!(
        "Nothing available can do the {} job. Check Settings: every model may be switched off or \
         unreachable.",
        role.as_str()
    )
}

/// Check an address, and add the one thing people always leave off.
///
/// A model running on your own machine always answers under `/v1`, and nobody
/// types that, so it is added for a local address and only for a local address.
/// A hosted service mounts its endpoint wherever it likes (Groq under
/// `/openai/v1`, Perplexity at the root), so guessing there would break more
/// than it fixed, and a cloud address is taken exactly as given.
pub fn parse_base_url(s: &str) -> Result<String> {
    let trimmed = s.trim().trim_end_matches('/');
    let u = url::Url::parse(trimmed).map_err(|_| {
        anyhow!("'{trimmed}' is not an address. It should look like http://127.0.0.1:11434")
    })?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err(anyhow!("An address must start with http:// or https://"));
    }
    if u.host_str().is_none() {
        return Err(anyhow!("'{trimmed}' has no host in it."));
    }

    let bare = u.path() == "/" || u.path().is_empty();
    if bare && is_local_url(trimmed) {
        return Ok(format!("{trimmed}/v1"));
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(id: &str, kind: Kind, url: Option<&str>, enabled: bool) -> Provider {
        Provider {
            id: id.into(),
            kind: kind.as_str().into(),
            label: id.into(),
            base_url: url.map(str::to_string),
            model: None,
            enabled,
            discovered: false,
            health: None,
            health_detail: None,
        }
    }

    #[test]
    fn only_the_claude_tool_can_carry_out_a_task() {
        // Saying otherwise would be a lie a user discovers at 08:00 on a
        // Wednesday, when the booking did not happen.
        assert!(Kind::ClaudeCli.can_carry_out_tasks());
        assert!(!Kind::OpenAiCompat.can_carry_out_tasks());
        assert!(!Kind::AnthropicApi.can_carry_out_tasks());
    }

    #[test]
    fn a_local_model_is_refused_the_executor_job_with_a_reason() {
        let local = p(
            "ollama",
            Kind::OpenAiCompat,
            Some("http://127.0.0.1:11434"),
            true,
        );
        let why = local.cannot_fill(Role::Executor).unwrap();
        assert!(why.contains("many turns"));
        assert!(
            why.contains("other three jobs"),
            "it must say what it CAN do: {why}"
        );
        // But it is perfectly good at the rest.
        assert!(local.cannot_fill(Role::Narrator).is_none());
        assert!(local.cannot_fill(Role::Fixer).is_none());
    }

    #[test]
    fn what_counts_as_staying_on_your_machine() {
        assert!(is_local_url("http://127.0.0.1:11434"));
        assert!(is_local_url("http://192.168.1.50:8080")); // scrub:allow private-ip a LAN model is local
        assert!(is_local_url("http://olares.local:8000"));
        assert!(!is_local_url("https://api.openai.com/v1"));
        assert!(!is_local_url("not a url"));

        // The Claude tool runs on your machine but talks to Anthropic, so it is
        // not local however it feels.
        assert!(!Kind::ClaudeCli.is_local(None));
    }

    #[test]
    fn the_chosen_provider_is_tried_first_then_the_others() {
        let providers = vec![
            p("claude", Kind::ClaudeCli, None, true),
            p(
                "ollama",
                Kind::OpenAiCompat,
                Some("http://127.0.0.1:11434"),
                true,
            ),
        ];
        let bindings = vec![(Role::Narrator, "ollama".to_string())];
        let chain = resolve_chain(&providers, &bindings, Role::Narrator, false);
        assert_eq!(chain[0].id, "ollama", "the choice comes first");
        assert_eq!(
            chain[1].id, "claude",
            "and something else is still a fallback"
        );
    }

    #[test]
    fn a_local_only_task_will_not_quietly_use_a_cloud_fallback() {
        let providers = vec![
            p("claude", Kind::ClaudeCli, None, true),
            p(
                "ollama",
                Kind::OpenAiCompat,
                Some("http://127.0.0.1:11434"),
                true,
            ),
        ];
        let chain = resolve_chain(&providers, &[], Role::Narrator, true);
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].id, "ollama");
    }

    #[test]
    fn a_local_only_task_that_needs_a_browser_has_nowhere_to_go_and_says_so() {
        let providers = vec![p(
            "ollama",
            Kind::OpenAiCompat,
            Some("http://127.0.0.1:11434"),
            true,
        )];
        let chain = resolve_chain(&providers, &[], Role::Executor, true);
        assert!(chain.is_empty());
        let why = explain_empty_chain(Role::Executor, true, &providers);
        assert!(why.contains("your own machine"));
        assert!(why.contains("Settings"));
    }

    #[test]
    fn a_switched_off_provider_is_never_used() {
        let providers = vec![p("claude", Kind::ClaudeCli, None, false)];
        assert!(resolve_chain(&providers, &[], Role::Narrator, false).is_empty());
    }

    #[test]
    fn with_nothing_configured_the_advice_is_how_to_get_started() {
        let why = explain_empty_chain(Role::Executor, false, &[]);
        assert!(why.contains("claude /login"));
    }

    #[test]
    fn no_machine_address_is_shipped_as_a_default() {
        // A build that goes out to strangers must not carry anybody's machine
        // address in it. Scanning is done by port, on this machine only, and
        // the only fixed addresses in the catalogue are loopback ones.
        for probe in PROBES {
            assert!(probe.port > 0);
        }
        for k in CATALOGUE {
            let u = url::Url::parse(k.base_url).unwrap();
            let host = u.host_str().unwrap_or_default();
            if is_local_url(k.base_url) {
                assert!(
                    host == "127.0.0.1" || host == "localhost",
                    "{} points at a specific machine: {host}",
                    k.id
                );
            } else {
                // A hosted service is a public name, never a bare address.
                assert!(
                    host.parse::<std::net::IpAddr>().is_err(),
                    "{} is a raw IP address, which is somebody's server",
                    k.id
                );
            }
        }
    }

    #[test]
    fn an_address_is_checked_before_it_is_saved() {
        assert!(parse_base_url("ollama").is_err());
        assert!(parse_base_url("file:///etc/passwd").is_err());
        assert!(parse_base_url("http://").is_err());
    }

    #[test]
    fn a_local_address_gets_the_bit_nobody_types() {
        // Every local runner answers under /v1 and nobody writes it, so typing
        // the address off the Ollama readme has to just work.
        assert_eq!(
            parse_base_url("http://127.0.0.1:11434").unwrap(),
            "http://127.0.0.1:11434/v1"
        );
        assert_eq!(
            parse_base_url("http://127.0.0.1:11434/").unwrap(),
            "http://127.0.0.1:11434/v1"
        );
        // Already said, so not said twice.
        assert_eq!(
            parse_base_url("http://127.0.0.1:11434/v1").unwrap(),
            "http://127.0.0.1:11434/v1"
        );
    }

    #[test]
    fn a_hosted_address_is_taken_exactly_as_given() {
        // Guessing here would break more than it fixed: Groq mounts under
        // /openai/v1 and Perplexity at the root, so there is nothing to guess.
        assert_eq!(
            parse_base_url("https://api.perplexity.ai").unwrap(),
            "https://api.perplexity.ai"
        );
        assert_eq!(
            parse_base_url("https://api.groq.com/openai/v1").unwrap(),
            "https://api.groq.com/openai/v1"
        );
    }

    #[test]
    fn every_service_in_the_catalogue_is_usable_as_written() {
        // A wrong entry here is worse than a missing one: the person pastes a
        // valid key, gets a failure, and blames their key.
        for k in CATALOGUE {
            assert!(
                !k.id.is_empty() && !k.name.is_empty(),
                "{} needs a name",
                k.id
            );
            let u = url::Url::parse(k.base_url)
                .unwrap_or_else(|_| panic!("{} has an unparseable address: {}", k.id, k.base_url));
            assert!(
                matches!(u.scheme(), "http" | "https"),
                "{} must be reached over http or https",
                k.id
            );
            assert!(
                !k.base_url.ends_with('/'),
                "{} must not end in a slash, or the path is built with two",
                k.id
            );
            assert!(
                !k.base_url.ends_with("/chat/completions"),
                "{} should be the root, not the endpoint",
                k.id
            );
            // A hosted service is taken verbatim, so its entry has to already be
            // complete rather than relying on the local-address fixup.
            if !is_local_url(k.base_url) {
                assert_eq!(
                    parse_base_url(k.base_url).unwrap(),
                    k.base_url,
                    "{} would be altered on the way in",
                    k.id
                );
            }
            assert!(
                k.keys_url.starts_with("https://"),
                "{} must tell people where to get a key",
                k.id
            );
            assert!(
                k.needs_key || is_local_url(k.base_url),
                "{} is hosted, so it needs a key",
                k.id
            );
        }
    }

    #[test]
    fn no_two_services_share_an_id() {
        // Ids are keychain account names. A collision would hand one service
        // another one's key.
        let mut ids: Vec<&str> = CATALOGUE.iter().map(|k| k.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "two services share an id");
    }

    #[test]
    fn a_key_belongs_to_exactly_one_service() {
        assert_eq!(key_account("openai"), "provider-key-openai");
        assert_ne!(key_account("openai"), key_account("openrouter"));
    }

    #[test]
    fn the_services_people_actually_ask_for_are_there() {
        for id in [
            "openai",
            "google",
            "openrouter",
            "xai",
            "deepseek",
            "moonshot",
            "groq",
        ] {
            assert!(known(id).is_some(), "{id} is missing from the catalogue");
        }
        // Anthropic is deliberately absent: it does not speak this format, and
        // Errand talks to it properly through its own transport instead.
        assert!(known("anthropic").is_none());
    }

    #[test]
    fn a_role_that_is_not_used_says_so() {
        // The rule that keeps this honest: if a role is not consulted, it owes
        // the interface a reason, and if it is consulted it must not claim one.
        for r in Role::ALL {
            assert_eq!(
                r.is_wired(),
                r.not_wired_reason().is_none(),
                "{} disagrees with itself about whether it is used",
                r.as_str()
            );
        }
        // Every role is consulted now, the planner included: it writes the plan
        // when a run finishes without leaving one of its own.
        for r in Role::ALL {
            assert!(r.is_wired(), "{} is offered but never asked", r.as_str());
        }
    }

    #[test]
    fn every_role_explains_itself() {
        for r in Role::ALL {
            let d = r.describe();
            assert!(d.len() > 40, "{r:?} needs a real explanation");
        }
        assert!(Role::Executor.describe().contains("Claude"));
        assert!(Role::Narrator.describe().contains("own machine"));
    }
}

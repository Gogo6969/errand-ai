//! Where the thinking happens.
//!
//! Errand does not ship with an AI. It uses whichever ones you already have,
//! and it is deliberately honest about what each can be trusted with.
//!
//! **Carrying out a task** means driving a browser over many turns: reading a
//! page, deciding what to click, calling a tool to do it. Errand owns that
//! loop, the tools it offers, and the budget and fence around them, so the one
//! thing it needs from a model is tool calling. Any model that can do that can
//! carry out a task, whether it is the Claude command line tool, a service you
//! pay for, or the machine under your desk.
//!
//! **The other three jobs** are one question with one answer: diagnose this
//! failure, summarise this run, distil these steps. Any competent model can do
//! that, and there is no reason to send it to a cloud.
//!
//! So being able to carry out a task is a property of the MODEL, not of the
//! sort of endpoint it sits behind, and this module carries what is actually
//! known about each one: that it used a tool when asked, that it would not, or
//! that nobody has looked yet. That third answer is the common one on a fresh
//! install, and reporting it as "cannot" is what had Errand refusing four
//! perfectly capable machines on somebody's own network.
//!
//! None of this claims every capable model is a good idea. A small model will
//! call the wrong tool, misread a page and give up half way through a booking.
//! It can be offered the job without being recommended for it.

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

    /// What is known about tool calling before any model is asked.
    ///
    /// Only the command line tool answers here: it is an agent loop in its own
    /// right, so whether it can call tools is not a question. Everything else
    /// has to be asked, and until it is, the honest answer is that nobody knows.
    pub fn tools_by_nature(&self) -> Option<Tools> {
        match self {
            Self::ClaudeCli => Some(Tools::Yes),
            Self::AnthropicApi | Self::OpenAiCompat => None,
        }
    }

    /// Why Errand cannot hand this kind a whole task, whatever its model can do.
    ///
    /// One case, and the obstacle is Errand's rather than the model's: Anthropic
    /// API models use tools perfectly well, but they are asked in Anthropic's
    /// own language and Errand's task loop speaks the OpenAI tool format. Saying
    /// that plainly is fairer than blaming the model.
    pub fn cannot_drive_a_task(&self) -> Option<&'static str> {
        match self {
            Self::AnthropicApi => Some(
                "Errand carries out tasks over the OpenAI tool format, and it speaks to \
                 Anthropic's API in Anthropic's own language, so it cannot drive this one \
                 through a task yet. Use the Claude command line tool, or reach the same model \
                 through a service that speaks the OpenAI format.",
            ),
            Self::ClaudeCli | Self::OpenAiCompat => None,
        }
    }

    /// Does anything leave your machine when this is used?
    pub fn is_local(&self, base_url: Option<&str>) -> bool {
        match self {
            Self::ClaudeCli | Self::AnthropicApi => false,
            Self::OpenAiCompat => base_url.map(is_local_url).unwrap_or(false),
        }
    }
}

/// Whether a model can call tools, which is the whole of what carrying out a
/// task asks of it.
///
/// Three-valued because the truth is three-valued. "Nobody has asked yet" is a
/// real answer, and the most common one, and folding it into "cannot" is how a
/// screen ends up calling a capable model incapable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tools {
    /// It was asked, and it called the tool.
    Yes,
    /// It was asked, and it answered in words instead.
    No,
    /// Nobody has asked it.
    #[default]
    Unknown,
}

impl Tools {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::No => "no",
            Self::Unknown => "unknown",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "yes" => Self::Yes,
            "no" => Self::No,
            "unknown" => Self::Unknown,
            _ => return None,
        })
    }

    /// Said in the interface, as a standing label rather than as evidence.
    ///
    /// What happened when a model was actually asked belongs in its health
    /// detail, next to everything else that came back from it.
    pub fn describe(&self) -> &'static str {
        match self {
            Self::Yes => "Can carry out tasks",
            Self::No => "Answers questions, but cannot use tools",
            Self::Unknown => "Not checked for tool use yet",
        }
    }
}

/// What Errand has learned about each endpoint, by provider id.
///
/// Kept apart from the provider itself because it is something Errand found
/// out, not something anybody typed in: a person configures an address and a
/// model name, and this is the app's own note of what happened when it asked.
pub type ToolsSeen = std::collections::BTreeMap<String, Tools>;

/// Where that note is written down.
pub const TOOLS_SEEN_KEY: &str = "ai.tools_seen";

/// Read it back, forgiving whatever else is in there.
///
/// Lenient on purpose: an entry written by a newer version, or one left behind
/// by a model that has since been removed, must not throw away everything else
/// that was learned.
pub fn read_tools_seen(stored: Option<&serde_json::Value>) -> ToolsSeen {
    let mut seen = ToolsSeen::new();
    let Some(obj) = stored.and_then(|v| v.as_object()) else {
        return seen;
    };
    for (id, value) in obj {
        if let Some(t) = value.as_str().and_then(Tools::parse) {
            seen.insert(id.clone(), t);
        }
    }
    seen
}

/// The same note, ready to be written back.
pub fn write_tools_seen(seen: &ToolsSeen) -> serde_json::Value {
    serde_json::Value::Object(
        seen.iter()
            .map(|(id, t)| (id.clone(), serde_json::Value::String(t.as_str().into())))
            .collect(),
    )
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

    /// The job in plain words, for a sentence that has to name it.
    pub fn plain(&self) -> &'static str {
        match self {
            Self::Executor => "carrying out the task",
            Self::Planner => "writing down what it learned",
            Self::Fixer => "working out why something failed",
            Self::Narrator => "writing the message you get",
        }
    }

    /// Said in the interface, so nobody has to guess what a role is for.
    pub fn describe(&self) -> &'static str {
        match self {
            Self::Executor => {
                "Actually does the task: opens the browser, signs in, decides what to click. \
                 Errand hands it the browser as tools and runs the loop itself, so any model \
                 that can call tools can do this job. A model that only answers questions \
                 cannot drive a browser."
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
    /// What Errand knows about this endpoint's tool calling.
    ///
    /// The kind answers first where it can, and otherwise this is whatever was
    /// written down the last time the model itself was asked.
    pub fn tools(&self, seen: &ToolsSeen) -> Tools {
        if let Some(settled) = self.kind_enum().and_then(|k| k.tools_by_nature()) {
            return settled;
        }
        seen.get(&self.id).copied().unwrap_or_default()
    }

    /// Why this model cannot be handed a whole task, if it cannot.
    ///
    /// Not knowing is not a reason to refuse. An unchecked model is offered the
    /// job, because the alternative is telling somebody their perfectly good
    /// model cannot do something nobody has ever asked it to do.
    pub fn cannot_carry_out_tasks(&self, tools: Tools) -> Option<String> {
        let Some(kind) = self.kind_enum() else {
            return Some(format!("Errand does not recognise what {} is.", self.label));
        };
        if let Some(why) = kind.cannot_drive_a_task() {
            return Some(why.to_string());
        }
        match tools {
            Tools::No => Some(format!(
                "{} answers questions, but it did not use the tools Errand gave it, so it cannot \
                 drive a browser. It can still do any of the other three jobs.",
                self.label
            )),
            Tools::Yes | Tools::Unknown => None,
        }
    }

    /// Why this provider cannot fill this role, if it cannot.
    pub fn cannot_fill(&self, role: Role, tools: Tools) -> Option<String> {
        if role.needs_agentic() {
            if let Some(why) = self.cannot_carry_out_tasks(tools) {
                return Some(why);
            }
        }
        if !self.enabled {
            return Some(format!("{} is switched off.", self.label));
        }
        None
    }
}

// ------------------------------------------------------- one task's choice --
//
// A model is chosen for a job once, on the AI screen, and that is the right
// default. It is the wrong answer for every task at once, though, because
// whichever model carries a task out is the model that reads whatever the tools
// hand back: a task that opens a mailbox wants one on your own machine, and a
// task that books a tennis court may as well use the best one you have. So a
// task may name its own, and saying nothing means following the default.

/// Which of the four jobs a task may name a model for.
///
/// One, deliberately. The job that matters is the one that sees what the task
/// sees; the other three read a finished run, and nobody has asked to vary
/// those per task. The column is a bag keyed by role, so a second one could be
/// added later without touching the schema, which is why the key is written
/// down rather than assumed.
fn task_model_key() -> &'static str {
    Role::Executor.as_str()
}

/// Which model a task has been told to carry itself out with.
///
/// `None` means it never said, and the choice on the AI screen stands. An
/// unreadable column means the same: a task whose stored settings cannot be
/// parsed should run on the default rather than not run at all.
pub fn read_task_model(stored: Option<&str>) -> Option<String> {
    let id = serde_json::from_str::<serde_json::Value>(stored?)
        .ok()?
        .get(task_model_key())?
        .as_str()?
        .trim()
        .to_string();
    (!id.is_empty()).then_some(id)
}

/// What to store for a task that has just been told which model to use.
///
/// Rewritten rather than replaced, so naming a model cannot take anything else
/// in that column with it. `None` clears the choice and puts the task back on
/// the default; an object left with nothing in it becomes `None`, so a task
/// that has said nothing stores nothing.
pub fn write_task_model(stored: Option<&str>, chosen: Option<&str>) -> Option<String> {
    let mut obj = stored
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    match chosen.map(str::trim).filter(|id| !id.is_empty()) {
        Some(id) => {
            obj.insert(task_model_key().to_string(), serde_json::Value::from(id));
        }
        None => {
            obj.remove(task_model_key());
        }
    }
    (!obj.is_empty()).then(|| serde_json::Value::Object(obj).to_string())
}

/// What an edit says about the model that carries a task out.
///
/// Three answers rather than two, because "the edit did not mention it" and
/// "put it back on the default" are different instructions and only one of them
/// should change anything. Without the distinction, saving a task's sites would
/// quietly forget which model it was told to use.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ModelChoice {
    #[default]
    Unchanged,
    /// Follow whatever the AI screen is set to.
    Default,
    /// This model in Errand's list, by id.
    Named(String),
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

// ------------------------------------------------- which Claude, exactly --
//
// The Claude command line tool is one endpoint that answers to three models,
// so "which model is Errand using" has no answer in its provider row: the
// answer is per job. This is where that choice lives, so the screen and the
// run read it from the same place and cannot disagree.

/// One of the Claude models the command line tool will answer to.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ClaudeModel {
    /// The name passed to the tool. An alias rather than a dated id
    /// ("sonnet", not "claude-sonnet-4-5-20250929") because the alias keeps
    /// working when a new version lands, and a dated id stops existing at
    /// seven in the morning in a run nobody is watching.
    pub alias: &'static str,
    /// What it is called where a person reads it.
    pub name: &'static str,
    /// What picking it means, said so somebody can act on it.
    pub what_it_is_for: &'static str,
}

/// The three that exist, biggest first.
pub const CLAUDE_MODELS: &[ClaudeModel] = &[
    ClaudeModel {
        alias: "opus",
        name: "Opus",
        what_it_is_for: "The biggest. Best at working out a page it has never seen before, and \
                         the most expensive to run.",
    },
    ClaudeModel {
        alias: "sonnet",
        name: "Sonnet",
        what_it_is_for: "The middle one, and what Errand uses for carrying out a task unless you \
                         say otherwise.",
    },
    ClaudeModel {
        alias: "haiku",
        name: "Haiku",
        what_it_is_for: "The smallest and cheapest. Fine for summarising a run or explaining why \
                         one failed; it will struggle to find its way round an unfamiliar site.",
    },
];

/// The model behind a name, if it is one Errand can ask for.
pub fn claude_model(alias: &str) -> Option<&'static ClaudeModel> {
    CLAUDE_MODELS.iter().find(|m| m.alias == alias)
}

/// The name to show for a model, so a sentence written for a person does not
/// read "sonnet" in the middle of it.
pub fn claude_model_name(alias: &str) -> &str {
    claude_model(alias).map(|m| m.name).unwrap_or(alias)
}

/// Which Claude each job has been set to use, by role name.
///
/// A settings entry rather than a column on the provider, because the provider
/// is one row and the choice is per job: the same tool is asked for Opus while
/// it drives a browser and Haiku while it writes the summary afterwards.
pub type ClaudeModels = std::collections::BTreeMap<String, String>;

/// Where that choice is written down.
pub const CLAUDE_MODELS_KEY: &str = "ai.claude_models";

/// Read it back, keeping only what can actually be asked for.
///
/// Anything unrecognised is dropped rather than carried through, so the worst a
/// stale or hand-edited entry can do is put a job back on its default. Passing
/// a name the tool does not know would fail the run instead.
pub fn read_claude_models(stored: Option<&serde_json::Value>) -> ClaudeModels {
    let mut chosen = ClaudeModels::new();
    let Some(obj) = stored.and_then(|v| v.as_object()) else {
        return chosen;
    };
    for (role, value) in obj {
        if Role::parse(role).is_none() {
            continue;
        }
        if let Some(m) = value.as_str().and_then(claude_model) {
            chosen.insert(role.clone(), m.alias.to_string());
        }
    }
    chosen
}

/// The same choice, ready to be written back.
pub fn write_claude_models(chosen: &ClaudeModels) -> serde_json::Value {
    serde_json::Value::Object(
        chosen
            .iter()
            .map(|(role, alias)| (role.clone(), serde_json::Value::String(alias.clone())))
            .collect(),
    )
}

/// Which Claude this job asks for: the choice if there is one, the default if
/// not.
///
/// Always one of the three, whatever is in the settings entry, because this is
/// what reaches the command line tool.
pub fn claude_model_for(role: Role, chosen: &ClaudeModels) -> &'static str {
    chosen
        .get(role.as_str())
        .and_then(|alias| claude_model(alias))
        .map(|m| m.alias)
        .unwrap_or_else(|| default_model_for(role))
}

/// What to say next to the Claude command line tool in a list of models.
///
/// Grouped by model rather than listed job by job, because four lines of "x
/// does y" is a paragraph and the question actually being asked is only ever
/// "which one is it".
pub fn claude_models_summary(chosen: &ClaudeModels) -> String {
    let mut groups: Vec<(&'static str, Vec<&'static str>)> = vec![];
    for role in Role::ALL {
        let name = claude_model_name(claude_model_for(role, chosen));
        match groups.iter_mut().find(|(n, _)| *n == name) {
            Some((_, jobs)) => jobs.push(role.plain()),
            None => groups.push((name, vec![role.plain()])),
        }
    }
    if let [(only, _)] = groups.as_slice() {
        return (*only).to_string();
    }
    groups
        .iter()
        .map(|(name, jobs)| format!("{name} for {}", listed_plainly(jobs)))
        .collect::<Vec<_>>()
        .join("; ")
}

/// "a", "a and b", "a, b and c".
fn listed_plainly(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Where a role's question should go, given what is configured.
///
/// Returns the chain to try in order. A chain rather than one answer because a
/// local model that is switched off should fall back rather than fail the run.
///
/// Knows nothing about which models have been asked to use a tool, which is
/// exactly right for the three jobs that are one question and one answer: tool
/// calling never comes into those. For the job that does need it, use
/// `resolve_chain_knowing` and hand over what has been learned, or a model
/// already proved incapable would be offered the work again.
pub fn resolve_chain<'a>(
    providers: &'a [Provider],
    bindings: &[(Role, String)],
    role: Role,
    local_only: bool,
) -> Vec<&'a Provider> {
    resolve_chain_knowing(providers, bindings, role, local_only, &ToolsSeen::new())
}

/// The same, told what each model turned out to be able to do.
pub fn resolve_chain_knowing<'a>(
    providers: &'a [Provider],
    bindings: &[(Role, String)],
    role: Role,
    local_only: bool,
    seen: &ToolsSeen,
) -> Vec<&'a Provider> {
    let mut chain: Vec<&Provider> = vec![];

    // What the person chose for this role, first.
    for (r, pid) in bindings {
        if *r != role {
            continue;
        }
        if let Some(p) = providers.iter().find(|p| &p.id == pid) {
            if p.cannot_fill(role, p.tools(seen)).is_none() {
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
        if p.cannot_fill(role, p.tools(seen)).is_some() {
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
                install it and run 'claude /login' once, or add a model of your own on the AI \
                 screen."
            .into();
    }
    if local_only {
        return format!(
            "This task is set to stay on your own machine, and nothing on it can do the job of \
             {}. Add a model of your own on the AI screen, or turn off 'Keep everything on \
             this machine', which is one switch for all tasks rather than one per task.",
            role.plain()
        );
    }
    if role.needs_agentic() {
        return "Nothing Errand can reach is able to carry out a task. Any model that can use \
                tools can do this: add one on the AI screen and choose it for \"Doing \
                the task\", or install the Claude command line tool and run 'claude /login' once."
            .into();
    }
    format!(
        "Nothing available can do the job of {}. Check Settings: every model may be switched off \
         or unreachable.",
        role.plain()
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

    fn local() -> Provider {
        p(
            "ollama",
            Kind::OpenAiCompat,
            Some("http://127.0.0.1:11434"),
            true,
        )
    }

    fn seen(id: &str, tools: Tools) -> ToolsSeen {
        ToolsSeen::from([(id.to_string(), tools)])
    }

    #[test]
    fn a_model_nobody_has_asked_is_not_reported_as_incapable() {
        // The complaint this whole change exists for: four capable machines on
        // somebody's network, every one of them greyed out as unable to do the
        // one job that matters, because nothing had ever asked them.
        let never_asked = local();
        assert_eq!(never_asked.tools(&ToolsSeen::new()), Tools::Unknown);
        assert!(
            never_asked.cannot_carry_out_tasks(Tools::Unknown).is_none(),
            "not having checked is not the same as having found out it cannot"
        );
        assert!(never_asked
            .cannot_fill(Role::Executor, Tools::Unknown)
            .is_none());
        assert!(
            Tools::Unknown.describe().contains("Not checked"),
            "the standing label has to say it is unchecked, not that it cannot"
        );
    }

    #[test]
    fn a_model_that_can_use_tools_may_carry_out_the_task_wherever_it_runs() {
        let desk = local();
        assert_eq!(desk.tools(&seen("ollama", Tools::Yes)), Tools::Yes);
        assert!(desk.cannot_fill(Role::Executor, Tools::Yes).is_none());
        // And the command line tool never needs asking: it is an agent loop.
        let cli = p("claude", Kind::ClaudeCli, None, true);
        assert_eq!(cli.tools(&ToolsSeen::new()), Tools::Yes);
    }

    #[test]
    fn a_model_that_would_not_use_tools_is_refused_and_told_why_about_itself() {
        let desk = local();
        let why = desk.cannot_fill(Role::Executor, Tools::No).unwrap();
        assert!(
            why.contains("did not use the tools"),
            "the reason has to be about this model, not about what kind of thing it is: {why}"
        );
        assert!(
            !why.contains("Claude"),
            "carrying out a task no longer requires Claude, so the reason must not say it: {why}"
        );
        assert!(
            why.contains("other three jobs"),
            "it must say what it CAN do: {why}"
        );
        // And it is still perfectly good at the rest.
        assert!(desk.cannot_fill(Role::Narrator, Tools::No).is_none());
        assert!(desk.cannot_fill(Role::Fixer, Tools::No).is_none());
    }

    #[test]
    fn anthropics_own_api_is_refused_for_a_reason_that_blames_errand_not_the_model() {
        // The model behind it uses tools perfectly well. Errand's task loop
        // speaks the other format, and pretending otherwise would send somebody
        // off to find a better model when there is nothing wrong with theirs.
        let api = p("anthropic-api", Kind::AnthropicApi, None, true);
        let why = api.cannot_carry_out_tasks(Tools::Yes).unwrap();
        assert!(why.contains("Errand"), "{why}");
        assert!(why.contains("OpenAI tool format"), "{why}");
        assert!(api.cannot_fill(Role::Narrator, Tools::Unknown).is_none());
    }

    #[test]
    fn what_was_learned_survives_a_round_trip_and_a_stray_entry() {
        let mut learned = ToolsSeen::new();
        learned.insert("desk".into(), Tools::Yes);
        learned.insert("tiny".into(), Tools::No);
        let stored = write_tools_seen(&learned);
        assert_eq!(read_tools_seen(Some(&stored)), learned);

        // Something a newer version wrote must not lose everything else.
        let mixed = serde_json::json!({ "desk": "yes", "future": "maybe", "n": 3 });
        let back = read_tools_seen(Some(&mixed));
        assert_eq!(back.get("desk"), Some(&Tools::Yes));
        assert_eq!(back.len(), 1);
        assert!(read_tools_seen(None).is_empty());
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
    fn a_local_only_task_can_be_carried_out_by_the_model_on_your_own_machine() {
        // What the person was promised and did not get. Nothing here is Claude,
        // and the task still has somewhere to go.
        let providers = vec![local()];
        let chain = resolve_chain_knowing(
            &providers,
            &[],
            Role::Executor,
            true,
            &seen("ollama", Tools::Yes),
        );
        assert_eq!(
            chain.len(),
            1,
            "a capable local model must be offered the job"
        );
        assert_eq!(chain[0].id, "ollama");
    }

    #[test]
    fn a_model_proved_incapable_is_left_out_of_the_chain_with_something_to_say() {
        let providers = vec![local()];
        let chain = resolve_chain_knowing(
            &providers,
            &[],
            Role::Executor,
            true,
            &seen("ollama", Tools::No),
        );
        assert!(
            chain.is_empty(),
            "a model that would not use tools cannot be the executor"
        );
        let why = explain_empty_chain(Role::Executor, true, &providers);
        assert!(why.contains("your own machine"));
        assert!(why.contains("AI screen"));
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
    fn nothing_capable_switched_on_reads_as_a_choice_rather_than_a_dead_end() {
        // It used to say the job needed the Claude command line tool. It does
        // not, and sending somebody off to install one when the model on their
        // desk would do is the exact complaint this change answers.
        let providers = vec![p("claude", Kind::ClaudeCli, None, false)];
        let why = explain_empty_chain(Role::Executor, false, &providers);
        assert!(
            why.contains("use tools"),
            "it must say what a model needs to be able to do: {why}"
        );
        assert!(
            !why.contains("needs the Claude"),
            "carrying out a task no longer requires Claude: {why}"
        );
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
    fn the_three_claudes_are_offered_by_the_names_the_tool_keeps_accepting() {
        // A dated id works until the day it does not, and that day arrives in
        // the middle of a run nobody is watching. Aliases do not expire.
        let offered: Vec<&str> = CLAUDE_MODELS.iter().map(|m| m.alias).collect();
        assert_eq!(offered, ["opus", "sonnet", "haiku"]);
        for m in CLAUDE_MODELS {
            assert!(!m.alias.contains('-'), "{} looks like a dated id", m.alias);
            assert!(
                !m.name.is_empty(),
                "{} needs a name a person reads",
                m.alias
            );
            assert!(
                m.what_it_is_for.len() > 40,
                "{} has to say what picking it means",
                m.alias
            );
        }
        assert!(claude_model("opus").is_some());
        assert!(claude_model("claude-3-opus-20240229").is_none());
        assert_eq!(claude_model_name("haiku"), "Haiku");
    }

    #[test]
    fn a_person_who_never_touches_this_keeps_exactly_the_models_they_had() {
        // The whole point of a default is that nothing changes for anybody who
        // does not care that the choice now exists.
        let untouched = ClaudeModels::new();
        assert_eq!(claude_model_for(Role::Executor, &untouched), "sonnet");
        assert_eq!(claude_model_for(Role::Planner, &untouched), "sonnet");
        assert_eq!(claude_model_for(Role::Fixer, &untouched), "haiku");
        assert_eq!(claude_model_for(Role::Narrator, &untouched), "haiku");
        for role in Role::ALL {
            assert_eq!(
                claude_model_for(role, &untouched),
                default_model_for(role),
                "{} changed under somebody who never chose anything",
                role.as_str()
            );
        }
    }

    #[test]
    fn choosing_a_model_for_one_job_leaves_the_other_three_alone() {
        let mut chosen = ClaudeModels::new();
        chosen.insert("executor".into(), "opus".into());
        assert_eq!(claude_model_for(Role::Executor, &chosen), "opus");
        assert_eq!(claude_model_for(Role::Narrator, &chosen), "haiku");

        // And it survives being written down and read back.
        let stored = write_claude_models(&chosen);
        assert_eq!(read_claude_models(Some(&stored)), chosen);
        assert!(read_claude_models(None).is_empty());
    }

    #[test]
    fn a_model_name_errand_cannot_ask_for_is_dropped_rather_than_passed_on() {
        // Whatever is in the settings entry, what reaches the command line tool
        // has to be a name it accepts, or the run fails at the moment the task
        // was meant to happen.
        let odd = serde_json::json!({
            "executor": "claude-4-ultra",
            "fixer": "opus",
            "gardener": "opus",
            "narrator": 3,
        });
        let back = read_claude_models(Some(&odd));
        assert_eq!(back.get("fixer"), Some(&"opus".to_string()));
        assert_eq!(
            back.len(),
            1,
            "only what can be asked for survives: {back:?}"
        );
        assert_eq!(
            claude_model_for(Role::Executor, &back),
            "sonnet",
            "a name that cannot be used falls back to the default"
        );
        for role in Role::ALL {
            assert!(
                claude_model(claude_model_for(role, &back)).is_some(),
                "{} would be asked for something that does not exist",
                role.as_str()
            );
        }
    }

    #[test]
    fn the_list_of_models_can_say_which_claude_is_doing_what() {
        // "Claude (command line tool)" with nothing after it is the complaint.
        // This is the line that answers it next to the name.
        let summary = claude_models_summary(&ClaudeModels::new());
        assert!(summary.contains("Sonnet"), "{summary}");
        assert!(summary.contains("Haiku"), "{summary}");
        assert!(
            summary.contains(Role::Executor.plain()),
            "it has to say which job gets the better model: {summary}"
        );

        // One model everywhere is said once, not four times.
        let mut all_opus = ClaudeModels::new();
        for role in Role::ALL {
            all_opus.insert(role.as_str().into(), "opus".into());
        }
        assert_eq!(claude_models_summary(&all_opus), "Opus");
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
        // The old wording said this job "must be Claude for now", which stopped
        // being true the moment Errand grew an agent loop of its own.
        let executor = Role::Executor.describe();
        assert!(executor.contains("call tools"), "{executor}");
        assert!(!executor.contains("Claude"), "{executor}");
        assert!(Role::Narrator.describe().contains("own machine"));
        for r in Role::ALL {
            assert!(!r.plain().is_empty(), "{r:?} needs a plain name");
        }
    }
}

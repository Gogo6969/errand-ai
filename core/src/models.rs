//! The canonical vocabulary.
//!
//! These enums serialize to exactly the strings stored in SQLite, sent on the
//! API wire, emitted as SSE events, and delivered to webhooks. One vocabulary,
//! no translation layer. That property is what the first review round found
//! specified three incompatible ways, so it is enforced here in types.

use serde::{Deserialize, Serialize};

// ------------------------------------------------------------------- tasks --

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Draft,
    Teaching,
    Ready,
    Paused,
    Archived,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Teaching => "teaching",
            Self::Ready => "ready",
            Self::Paused => "paused",
            Self::Archived => "archived",
        }
    }
}

// -------------------------------------------------------------------- runs --

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Armed,
    Queued,
    Preflight,
    Holding,
    Running,
    Healing,
    WaitingInput,
    Takeover,
    Succeeded,
    Failed,
    Cancelled,
    Skipped,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Armed => "armed",
            Self::Queued => "queued",
            Self::Preflight => "preflight",
            Self::Holding => "holding",
            Self::Running => "running",
            Self::Healing => "healing",
            Self::WaitingInput => "waiting_input",
            Self::Takeover => "takeover",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Skipped
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunTrigger {
    Schedule,
    Manual,
    Api,
    Teach,
    HealRetry,
    CatchUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    Normal,
    Teach,
    DryRun,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Plan,
    Navigate,
    Act,
    Read,
    Decide,
    Credential,
    Wait,
    Message,
    Screenshot,
    Heal,
    Intervention,
    Note,
}

/// The failure taxonomy. Every terminal failure carries one of these plus a
/// plain-language explanation, and the database rejects a `failed` run without
/// both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    AuthExpired,
    UiChanged,
    TargetUnavailable,
    CaptchaOr2faNeeded,
    Network,
    BudgetExceeded,
    NeedsHumanDecision,
    ProviderError,
    /// The agent was offered tools it must not have. Terminal and auto-pausing:
    /// retrying an unsafe spawn is worse than not running at all.
    ContainmentBreach,
    CrashDuringSideEffect,
    MissedWindow,
    MissedWhileAsleep,
    StillRunning,
    CancelledByUser,
}

/// How the retry ladder treats a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryClass {
    Transient,
    Healable,
    Terminal,
    None,
}

impl FailureCode {
    pub fn retry_class(&self) -> RetryClass {
        match self {
            Self::Network | Self::ProviderError => RetryClass::Transient,
            Self::UiChanged => RetryClass::Healable,
            Self::AuthExpired
            | Self::TargetUnavailable
            | Self::CaptchaOr2faNeeded
            | Self::BudgetExceeded
            | Self::NeedsHumanDecision
            | Self::ContainmentBreach
            | Self::CrashDuringSideEffect => RetryClass::Terminal,
            Self::MissedWindow
            | Self::MissedWhileAsleep
            | Self::StillRunning
            | Self::CancelledByUser => RetryClass::None,
        }
    }

    /// Auth failures pause the task so it does not fail the same way every day
    /// until someone notices.
    pub fn should_auto_pause(&self) -> bool {
        matches!(self, Self::AuthExpired | Self::ContainmentBreach)
    }
}

// ------------------------------------------------------------------- roles --

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Executor,
    Planner,
    Fixer,
    Narrator,
}

// ------------------------------------------------------------------ events --

/// The one event vocabulary. Emitted on the per-run SSE stream, the global
/// firehose, Tauri events, and webhooks, with identical names in all four.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data")]
pub enum Event {
    #[serde(rename = "run.status")]
    RunStatus {
        run_id: String,
        task_id: String,
        status: RunStatus,
    },
    #[serde(rename = "step.started")]
    StepStarted {
        run_id: String,
        seq: i64,
        kind: StepKind,
        title: String,
    },
    #[serde(rename = "step.finished")]
    StepFinished {
        run_id: String,
        seq: i64,
        kind: StepKind,
        title: String,
        ok: bool,
        duration_ms: Option<i64>,
    },
    #[serde(rename = "run.needs_attention")]
    RunNeedsAttention {
        run_id: String,
        task_id: String,
        question: String,
    },
    #[serde(rename = "run.finished")]
    RunFinished {
        run_id: String,
        task_id: String,
        status: RunStatus,
        summary: Option<String>,
    },
    #[serde(rename = "run.failed")]
    RunFailed {
        run_id: String,
        task_id: String,
        failure_code: FailureCode,
        failure_human: String,
    },
    #[serde(rename = "task.updated")]
    TaskUpdated { task_id: String },
}

impl Event {
    /// SSE event name, matching the serde tag exactly.
    pub fn name(&self) -> &'static str {
        match self {
            Self::RunStatus { .. } => "run.status",
            Self::StepStarted { .. } => "step.started",
            Self::StepFinished { .. } => "step.finished",
            Self::RunNeedsAttention { .. } => "run.needs_attention",
            Self::RunFinished { .. } => "run.finished",
            Self::RunFailed { .. } => "run.failed",
            Self::TaskUpdated { .. } => "task.updated",
        }
    }

    /// Run this event belongs to, for filtering the per-run stream.
    pub fn run_id(&self) -> Option<&str> {
        match self {
            Self::RunStatus { run_id, .. }
            | Self::StepStarted { run_id, .. }
            | Self::StepFinished { run_id, .. }
            | Self::RunNeedsAttention { run_id, .. }
            | Self::RunFinished { run_id, .. }
            | Self::RunFailed { run_id, .. } => Some(run_id),
            Self::TaskUpdated { .. } => None,
        }
    }
}

// ------------------------------------------------------------------ scopes --

/// API token scopes. `approve` is deliberately separate from `run`: approval
/// gates exist to put a human in front of an irreversible action, so a client
/// that can start a booking must not also be able to confirm it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Read,
    Run,
    Webhook,
    Approve,
    Manage,
    Admin,
}

impl Scope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Run => "run",
            Self::Webhook => "webhook",
            Self::Approve => "approve",
            Self::Manage => "manage",
            Self::Admin => "admin",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim() {
            "read" => Self::Read,
            "run" => Self::Run,
            "webhook" => Self::Webhook,
            "approve" => Self::Approve,
            "manage" => Self::Manage,
            "admin" => Self::Admin,
            _ => return None,
        })
    }
}

// ------------------------------------------------------------- wire objects --

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub name: String,
    pub emoji: Option<String>,
    pub description: String,
    pub understanding: Option<String>,
    pub status: String,
    pub schedule: serde_json::Value,
    pub notify: serde_json::Value,
    pub limits: serde_json::Value,
    pub allowed_domains: serde_json::Value,
    pub playbook_version: Option<i64>,
    pub next_run_at: Option<String>,
    pub paused_reason: Option<String>,
    pub auto_paused: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Failure {
    pub code: String,
    pub plain_reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub technical: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub task_id: String,
    pub occurrence_id: String,
    pub mode: String,
    pub trigger: String,
    pub triggered_by: Option<String>,
    pub status: String,
    pub scheduled_for: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cost_usd: f64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub seq: i64,
    pub ts: String,
    pub kind: String,
    pub title: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
}

/// Credential metadata. There is no field for the secret, at any scope, by
/// construction rather than by filtering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialMeta {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub domain: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    pub require_biometric: bool,
    pub last_used_at: Option<String>,
    pub use_count: i64,
    pub created_at: String,
}

/// Somebody a task may contact.
///
/// `address_masked` exists so the agent can be told who it is writing to
/// without being told how to reach them. It has no use for the real address —
/// the outbox does the sending — and an address sitting in a model's context is
/// an address that can leave in an answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recipient {
    pub id: String,
    pub label: String,
    pub channel: String,
    pub address: String,
    pub address_masked: String,
    pub created_at: String,
}

/// A recipient as granted to one task, with what that task may tell them.
///
/// The grant is per task rather than global, because "this task may contact
/// these people" is exactly the boundary somebody would want to reason about
/// before letting a task run unattended.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecipient {
    pub id: String,
    pub label: String,
    pub channel: String,
    pub address: String,
    pub address_masked: String,
    pub on_success: bool,
    pub on_failure: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Health {
    pub status: String,
    pub version: String,
    pub schema_version: i64,
    pub uptime_s: u64,
    pub busy_runs: i64,
    pub db: String,
    pub scheduler: String,
    /// "ok", "blocked", "error", or "checking". A blocked keychain means macOS
    /// is waiting on an authorization prompt nobody can see, so this is
    /// reported rather than allowed to look like a hang.
    pub keychain: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_strings_match_the_database_check_constraints() {
        assert_eq!(RunStatus::WaitingInput.as_str(), "waiting_input");
        assert_eq!(TaskStatus::Ready.as_str(), "ready");
        assert_eq!(Scope::Approve.as_str(), "approve");
    }

    #[test]
    fn event_name_matches_serde_tag() {
        let e = Event::TaskUpdated {
            task_id: "t".into(),
        };
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["event"], e.name());
    }

    #[test]
    fn auth_failure_pauses_the_task() {
        assert!(FailureCode::AuthExpired.should_auto_pause());
        assert!(!FailureCode::TargetUnavailable.should_auto_pause());
        assert_eq!(FailureCode::UiChanged.retry_class(), RetryClass::Healable);
    }
}

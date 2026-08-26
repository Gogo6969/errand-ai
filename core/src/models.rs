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

/// What a run is for, and whether anything it does is real.
///
/// Two answers rather than one, because they are two different questions.
/// Teaching a task and rehearsing it are not alternatives: a job that moves
/// somebody's post or books a court is precisely the one a person wants to
/// watch all the way through with nothing actually happening, and that run is
/// both learning the job and doing none of it. A single word can only say one
/// of the two, so the pair is chosen here, together, and nothing can store a
/// rehearsal that forgot to say it was one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunMode {
    stored: &'static str,
    rehearsal: bool,
}

impl RunMode {
    /// Doing the job, for real.
    pub const NORMAL: Self = Self {
        stored: "normal",
        rehearsal: false,
    };
    /// The supervised first run: it works the job out from the description and
    /// writes down a plan at the end for a person to approve.
    pub const TEACH: Self = Self {
        stored: "teach",
        rehearsal: false,
    };
    /// A run of a task that already knows the job, with everything irreversible
    /// recorded instead of done.
    pub const REHEARSAL: Self = Self {
        stored: "dry_run",
        rehearsal: true,
    };
    /// Teaching, rehearsed. It still ends with a plan to approve; it simply
    /// books, sends and moves nothing on the way there.
    pub const TEACH_REHEARSAL: Self = Self {
        stored: "teach",
        rehearsal: true,
    };

    /// Teaching, rehearsed or not, decided in one place so the two halves of
    /// the answer cannot drift apart at the two ends of an if.
    pub fn teach(rehearsal: bool) -> Self {
        if rehearsal {
            Self::TEACH_REHEARSAL
        } else {
            Self::TEACH
        }
    }

    /// An ordinary run, rehearsed or not.
    pub fn run(rehearsal: bool) -> Self {
        if rehearsal {
            Self::REHEARSAL
        } else {
            Self::NORMAL
        }
    }

    /// The word the mode column stores.
    pub fn stored(&self) -> &'static str {
        self.stored
    }

    pub fn is_rehearsal(&self) -> bool {
        self.rehearsal
    }
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
    /// Which model in Errand's list carries this task out, by id.
    ///
    /// Null means the task never said, so it follows the choice on the AI
    /// screen. It is worth naming one here because whichever model does the
    /// work is the model that reads whatever the tools hand back, and that is a
    /// per-task decision: a task that opens a mailbox is not the same as one
    /// that books a tennis court.
    pub model_id: Option<String>,
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
    /// One line: what stopped it. Written for a person, and short enough to
    /// read in a list without opening anything.
    pub plain_reason: String,
    /// One line: what they can do about it. Absent when there is nothing to
    /// do, which is better than inventing something.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub technical: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub task_id: String,
    pub occurrence_id: String,
    pub mode: String,
    /// Was everything irreversible recorded rather than done? Ask
    /// [`Run::is_rehearsal`] rather than reading this directly.
    pub rehearsal: bool,
    pub trigger: String,
    pub triggered_by: Option<String>,
    pub status: String,
    pub scheduled_for: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub summary: Option<String>,
    /// What the run produced: the answer, not the story of getting it.
    ///
    /// Always serialised, including as null, because "this run recorded no
    /// answer" is a fact a reader needs and an absent key looks like an older
    /// build.
    pub answer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cost_usd: f64,
    pub created_at: String,
}

impl Run {
    /// Is nothing this run does allowed to really happen?
    ///
    /// Every rehearsal check in the program comes through here, because there
    /// are two kinds of rehearsal and only one of them says so in its mode: a
    /// teach run somebody asked to rehearse is still called "teach", since
    /// learning is what it is for. Anything that read the mode for itself would
    /// answer no to that one and really book the court.
    pub fn is_rehearsal(&self) -> bool {
        self.rehearsal
    }

    /// Is this the supervised first run, the one that ends by writing down a
    /// plan for somebody to approve?
    ///
    /// True of a rehearsed teach as well. Rehearsing changes what a teach run
    /// does to the world, not what it is for.
    pub fn is_teaching(&self) -> bool {
        self.mode == RunMode::TEACH.stored()
    }
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

/// A file a run left behind, such as a screenshot. Addressed by id, never by
/// client-supplied filename, so being asked for one can never be turned into
/// reading an arbitrary path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub id: String,
    pub run_id: String,
    pub kind: String,
    pub rel_path: String,
    pub masked: bool,
    pub bytes: Option<i64>,
    pub created_at: String,
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
/// without being told how to reach them. It has no use for the real address, since
/// the outbox does the sending, and an address sitting in a model's context is
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

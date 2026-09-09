//! Durable execution-control types shared by storage, engines, and client surfaces.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Collaboration boundary enforced independently from the selected Agent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationMode {
    /// Read-only planning and durable Plan/Todo maintenance.
    Plan,
    /// Authorized implementation work.
    #[default]
    Work,
}

impl CollaborationMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Work => "work",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "plan" => Some(Self::Plan),
            "work" => Some(Self::Work),
            _ => None,
        }
    }
}

/// Exact Agent and model configuration frozen for one turn or continuation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnExecutionIdentity {
    pub agent: String,
    pub provider_id: String,
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

impl TurnExecutionIdentity {
    #[must_use]
    pub fn new(
        agent: impl Into<String>,
        provider_id: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Self {
        Self {
            agent: agent.into(),
            provider_id: provider_id.into(),
            model_id: model_id.into(),
            reasoning: None,
        }
    }

    #[must_use]
    pub fn with_reasoning(mut self, reasoning: Option<impl Into<String>>) -> Self {
        self.reasoning = reasoning.map(Into::into);
        self
    }
}

/// Explicit user-owned control operation admitted into a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserControlKind {
    StartPlan,
    StartWork,
    ResumeGoal,
}

/// Source of one automatically delivered terminal completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionSource {
    BackgroundExecution,
    AgentJob,
    Workflow,
    ProductAgent,
}

/// Why a turn starts and which durable contract supplies its identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum TurnStartKind {
    User,
    UserControl(UserControlKind),
    Automatic(CompletionSource),
    Recovery,
}

/// Durable continuation authority for recovery after compaction or restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinuationToken {
    pub cycle_id: String,
    pub identity: TurnExecutionIdentity,
    pub mode: CollaborationMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_revision: Option<i64>,
    pub context_epoch: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_message_id: Option<String>,
}

/// Explicit acceptance of starting implementation while a bound review remains Draft.
///
/// The runtime records the exact review revision and the user's reason so later recovery
/// cannot silently reinterpret a generic mode switch as review approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftReviewRiskAcceptance {
    pub review_id: String,
    pub review_revision: i64,
    pub reason: String,
    pub time_accepted: i64,
}

/// Durable phase of one session's execution controller.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionExecutionPhase {
    #[default]
    Idle,
    Planning,
    Authorized,
    Running,
    Waiting,
    Paused,
    Completed,
    Blocked,
}

impl SessionExecutionPhase {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Planning => "planning",
            Self::Authorized => "authorized",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Blocked => "blocked",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "idle" => Some(Self::Idle),
            "planning" => Some(Self::Planning),
            "authorized" => Some(Self::Authorized),
            "running" => Some(Self::Running),
            "waiting" => Some(Self::Waiting),
            "paused" => Some(Self::Paused),
            "completed" => Some(Self::Completed),
            "blocked" => Some(Self::Blocked),
            _ => None,
        }
    }
}

/// Authoritative execution state for one durable session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionExecutionState {
    pub session_id: String,
    pub revision: i64,
    pub mode: CollaborationMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_identity: Option<TurnExecutionIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorized_plan_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorized_plan_revision: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_plan_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_plan_revision: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_review_risk: Option<DraftReviewRiskAcceptance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cycle_id: Option<String>,
    pub phase: SessionExecutionPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<ContinuationToken>,
    pub time_created: i64,
    pub time_updated: i64,
}

/// Stable trigger spelling stored alongside one durable inbox input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputTriggerKind {
    #[default]
    Legacy,
    User,
    UserControl,
    Automatic,
    Recovery,
}

impl InputTriggerKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::User => "user",
            Self::UserControl => "user_control",
            Self::Automatic => "automatic",
            Self::Recovery => "recovery",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "legacy" => Some(Self::Legacy),
            "user" => Some(Self::User),
            "user_control" => Some(Self::UserControl),
            "automatic" => Some(Self::Automatic),
            "recovery" => Some(Self::Recovery),
            _ => None,
        }
    }
}

/// One terminal completion routed exactly once to a parent session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionEnvelope {
    pub source_key: String,
    pub source: CompletionSource,
    pub terminal_revision: u64,
    pub parent_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cycle_id: Option<String>,
    pub payload: Value,
}

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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
    ResumeWork,
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

/// The exact input a session is waiting for. A completion from another cycle
/// cannot satisfy an external wait, even if the external source ID is reused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum SessionWaitReference {
    Human {
        request_id: String,
    },
    External {
        source_id: String,
        origin_cycle_id: String,
    },
}

/// Why automatic execution stopped. This is host state, never inferred from
/// assistant prose or the existence of unfinished Plan steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPauseReason {
    NoProgress,
    NoExecutableWork,
    User,
    Authentication,
    TurnBudget,
    Blocked,
}

/// Session-level execution eligibility, independent of Plan and Goal status.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum SessionReadiness {
    #[default]
    Ready,
    WaitingHuman {
        request_id: String,
    },
    WaitingExternal {
        source_id: String,
        origin_cycle_id: String,
    },
    Paused {
        reason: SessionPauseReason,
    },
    Completed,
}

impl From<SessionWaitReference> for SessionReadiness {
    fn from(wait: SessionWaitReference) -> Self {
        match wait {
            SessionWaitReference::Human { request_id } => Self::WaitingHuman { request_id },
            SessionWaitReference::External {
                source_id,
                origin_cycle_id,
            } => Self::WaitingExternal {
                source_id,
                origin_cycle_id,
            },
        }
    }
}

/// Durable scheduling metadata on the existing execution-state row.
///
/// Progress belongs to the session, so a new callback or cycle does not erase
/// the evidence that caused a no-progress pause. Hosts update these fields when
/// they observe progress; admitting an input does not reset them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionScheduling {
    pub readiness: SessionReadiness,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_fingerprint: Option<String>,
    #[serde(default)]
    pub unchanged_progress_count: u32,
}

/// A host-verified wake source. Construct `UserAnswer` only after accepting that
/// request's answer, and `ExternalCompletion` only for an observed terminal
/// result. These signals do not themselves grant Plan/Goal or tool authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum SessionWakeSignal {
    UserQuery,
    UserAnswer {
        request_id: String,
    },
    ExplicitResume,
    ExternalCompletion {
        source_id: String,
        origin_cycle_id: String,
    },
    Automatic,
    Recovery,
    Callback,
}

/// Pure scheduling decision. Admission never changes execution authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeAdmission {
    Reject,
    /// Admit this input while preserving the existing gate and progress.
    /// A status query while paused/waiting must not resume automatic work.
    Admit,
    /// Clear only the gate satisfied by this signal, then admit the input.
    Resume,
}

impl SessionReadiness {
    /// Evaluate a wake without side effects or consulting Plan/Goal state.
    #[must_use]
    pub fn wake_admission(&self, signal: &SessionWakeSignal) -> WakeAdmission {
        use SessionWakeSignal as Wake;
        if matches!(signal, Wake::UserQuery) {
            return if matches!(self, Self::Completed) {
                WakeAdmission::Resume
            } else {
                WakeAdmission::Admit
            };
        }
        match (self, signal) {
            // A completed turn may receive a later, validated deferred answer.
            // It starts new session work without granting Plan/Goal authority.
            (Self::Completed, Wake::UserAnswer { request_id }) if !request_id.trim().is_empty() => {
                WakeAdmission::Resume
            }
            // A verified answer to a deferred question can arrive while ready;
            // there is no gate to clear. Request validation/deduplication is
            // still the host's responsibility.
            (Self::Ready, Wake::UserAnswer { request_id }) if request_id.trim().is_empty() => {
                WakeAdmission::Reject
            }
            (
                Self::Ready,
                Wake::ExternalCompletion {
                    source_id,
                    origin_cycle_id,
                },
            ) if source_id.trim().is_empty() || origin_cycle_id.trim().is_empty() => {
                WakeAdmission::Reject
            }
            (Self::Ready, _) => WakeAdmission::Admit,
            (
                Self::WaitingHuman { request_id },
                Wake::UserAnswer {
                    request_id: answered,
                },
            ) if !request_id.trim().is_empty() && request_id == answered => WakeAdmission::Resume,
            (
                Self::WaitingExternal {
                    source_id,
                    origin_cycle_id,
                },
                Wake::ExternalCompletion {
                    source_id: completed,
                    origin_cycle_id: completed_cycle,
                },
            ) if !source_id.trim().is_empty()
                && !origin_cycle_id.trim().is_empty()
                && source_id == completed
                && origin_cycle_id == completed_cycle =>
            {
                WakeAdmission::Resume
            }
            (Self::Paused { .. } | Self::Completed, Wake::ExplicitResume) => WakeAdmission::Resume,
            _ => WakeAdmission::Reject,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduling: Option<SessionScheduling>,
    pub time_created: i64,
    pub time_updated: i64,
}

impl SessionExecutionState {
    /// Apply the pure wake policy, including conservative handling of legacy
    /// rows with no scheduling metadata. An untyped legacy wait cannot be
    /// satisfied by a guessed answer or completion.
    #[must_use]
    pub fn wake_admission(&self, signal: &SessionWakeSignal) -> WakeAdmission {
        if let Some(scheduling) = &self.scheduling {
            return scheduling.readiness.wake_admission(signal);
        }
        match self.phase {
            SessionExecutionPhase::Waiting => {
                if matches!(signal, SessionWakeSignal::UserQuery) {
                    WakeAdmission::Admit
                } else {
                    WakeAdmission::Reject
                }
            }
            SessionExecutionPhase::Paused | SessionExecutionPhase::Blocked => {
                SessionReadiness::Paused {
                    reason: SessionPauseReason::User,
                }
                .wake_admission(signal)
            }
            SessionExecutionPhase::Completed => SessionReadiness::Completed.wake_admission(signal),
            _ => SessionReadiness::Ready.wake_admission(signal),
        }
    }
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

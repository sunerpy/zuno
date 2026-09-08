use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

pub const MAX_CLAIM_STATEMENT_CHARS: usize = 500;
pub const MAX_EVIDENCE_ANCHORS: usize = 4;
pub const MAX_LOAD_BEARING_CLAIMS: usize = 8;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ClaimKind {
    Fact,
    NegativeFact,
    Inference,
    Recommendation,
}

impl ClaimKind {
    pub const ALL: [Self; 4] = [
        Self::Fact,
        Self::NegativeFact,
        Self::Inference,
        Self::Recommendation,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::NegativeFact => "negative_fact",
            Self::Inference => "inference",
            Self::Recommendation => "recommendation",
        }
    }

    #[must_use]
    pub const fn requires_evidence(self) -> bool {
        !matches!(self, Self::Recommendation)
    }

    #[must_use]
    pub const fn requires_countercheck(self) -> bool {
        matches!(self, Self::NegativeFact)
    }
}

impl fmt::Display for ClaimKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClaimPriority {
    P0,
    P1,
    P2,
}

impl ClaimPriority {
    pub const ALL: [Self; 3] = [Self::P0, Self::P1, Self::P2];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::P0 => "p0",
            Self::P1 => "p1",
            Self::P2 => "p2",
        }
    }
}

impl fmt::Display for ClaimPriority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SystemLayer {
    DurableAdmission,
    RuntimeScheduling,
    ProviderTransport,
    RpcResponse,
    ClientProjection,
    UserInterface,
}

impl SystemLayer {
    pub const ALL: [Self; 6] = [
        Self::DurableAdmission,
        Self::RuntimeScheduling,
        Self::ProviderTransport,
        Self::RpcResponse,
        Self::ClientProjection,
        Self::UserInterface,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DurableAdmission => "durable_admission",
            Self::RuntimeScheduling => "runtime_scheduling",
            Self::ProviderTransport => "provider_transport",
            Self::RpcResponse => "rpc_response",
            Self::ClientProjection => "client_projection",
            Self::UserInterface => "user_interface",
        }
    }
}

impl fmt::Display for SystemLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClaimStatus {
    Unverified,
    Verified,
    Contested,
    Refuted,
    Stale,
}

impl ClaimStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unverified => "unverified",
            Self::Verified => "verified",
            Self::Contested => "contested",
            Self::Refuted => "refuted",
            Self::Stale => "stale",
        }
    }

    #[must_use]
    pub const fn may_be_relied_on(self) -> bool {
        matches!(self, Self::Verified)
    }
}

impl fmt::Display for ClaimStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CountercheckKind {
    DefinitionLookup,
    CallerCallee,
    AlternatePath,
    ContractOrTest,
    CounterexampleSearch,
}

impl CountercheckKind {
    pub const ALL: [Self; 5] = [
        Self::DefinitionLookup,
        Self::CallerCallee,
        Self::AlternatePath,
        Self::ContractOrTest,
        Self::CounterexampleSearch,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DefinitionLookup => "definition_lookup",
            Self::CallerCallee => "caller_callee",
            Self::AlternatePath => "alternate_path",
            Self::ContractOrTest => "contract_or_test",
            Self::CounterexampleSearch => "counterexample_search",
        }
    }
}

impl fmt::Display for CountercheckKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Countercheck {
    pub kind: CountercheckKind,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvidenceAnchor {
    pub path: String,
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub start_line: Option<u32>,
    #[serde(default)]
    pub end_line: Option<u32>,
    /// Filled by the host after reading the actual source.
    #[serde(default)]
    pub content_digest: Option<String>,
    #[serde(default)]
    pub reference: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "id")]
pub enum ActorRef {
    Parent,
    User,
    Delegate(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
pub struct CodeGraphIndexSnapshot {
    pub initialized: bool,
    pub extraction_status: String,
    pub built_with_extraction_version: u32,
    pub current_extraction_version: u32,
    pub pending_added: u32,
    pub pending_modified: u32,
    pub pending_removed: u32,
    pub worktree_mismatch: Option<String>,
    pub last_indexed_ms: Option<i64>,
}

impl CodeGraphIndexSnapshot {
    #[must_use]
    pub const fn pending_total(&self) -> u32 {
        self.pending_added
            .saturating_add(self.pending_modified)
            .saturating_add(self.pending_removed)
    }

    #[must_use]
    pub fn is_trustworthy(&self) -> bool {
        self.initialized
            && self.extraction_status == "current"
            && self.worktree_mismatch.is_none()
            && self.pending_total() == 0
    }

    #[must_use]
    pub fn untrustworthy_reason(&self) -> Option<String> {
        if !self.initialized {
            return Some("is not initialized for this project".to_owned());
        }
        if let Some(mismatch) = self.worktree_mismatch.as_deref() {
            return Some(format!("belongs to a different worktree ({mismatch})"));
        }
        if self.extraction_status != "current" {
            return Some(format!(
                "reports extraction status `{}` rather than `current`",
                self.extraction_status
            ));
        }
        match self.pending_total() {
            0 => None,
            pending => Some(format!(
                "has {pending} file(s) pending sync ({} added, {} modified, {} removed)",
                self.pending_added, self.pending_modified, self.pending_removed
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSourceSnapshot {
    pub id: String,
    pub repository_root: String,
    pub head_sha: String,
    pub branch: Option<String>,
    pub worktree_path: String,
    pub dirty: bool,
    /// SHA-256 of the scoped porcelain status and working-tree bytes.
    pub worktree_digest: String,
    pub scope_paths: Vec<String>,
    pub artifact: Option<ReviewArtifactSnapshot>,
    pub codegraph: CodeGraphIndexSnapshot,
    pub captured_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewArtifactSnapshot {
    pub path: String,
    pub content_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewReviewClaim {
    pub review_id: String,
    pub statement: String,
    pub kind: ClaimKind,
    pub priority: ClaimPriority,
    pub layer: Option<SystemLayer>,
    pub source_snapshot_id: String,
    pub evidence: Vec<EvidenceAnchor>,
    pub counterchecks: Vec<Countercheck>,
    pub asserted_by: ActorRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewClaim {
    pub id: String,
    pub review_id: String,
    pub statement: String,
    pub kind: ClaimKind,
    pub priority: ClaimPriority,
    pub layer: Option<SystemLayer>,
    pub source_snapshot_id: String,
    pub evidence: Vec<EvidenceAnchor>,
    pub counterchecks: Vec<Countercheck>,
    pub status: ClaimStatus,
    pub asserted_by: ActorRef,
    pub parent_verification: Option<ReviewVerificationReceipt>,
    pub time_created: i64,
    pub time_updated: i64,
}

impl ReviewClaim {
    #[must_use]
    pub fn missing_counterchecks(&self) -> Vec<CountercheckKind> {
        if !self.kind.requires_countercheck() {
            return Vec::new();
        }
        CountercheckKind::ALL
            .into_iter()
            .filter(|kind| !self.counterchecks.iter().any(|check| check.kind == *kind))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewVerificationReceipt {
    pub session_id: String,
    pub message_id: String,
    pub call_id: String,
    pub agent: String,
    pub time_verified: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewIssueKind {
    Contradiction,
    Unresolved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewIssue {
    pub id: String,
    pub kind: ReviewIssueKind,
    pub statement: String,
    pub detail: Vec<String>,
    pub resolved: bool,
    pub resolution: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewDelegateReceipt {
    pub run_id: String,
    pub job_id: String,
    pub preset: String,
    pub preset_source_id: String,
    pub seat_id: String,
    pub agent: String,
    pub source_snapshot_id: String,
    pub report_digest: String,
    pub time_imported: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewReceipt {
    pub id: String,
    pub review_revision: i64,
    pub source_snapshot_id: String,
    pub head_sha: String,
    pub worktree_digest: String,
    pub artifact_digest: Option<String>,
    #[serde(default)]
    pub evidence_digest: String,
    pub time_issued: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    Draft,
    Ready,
}

impl ReviewStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Ready => "ready",
        }
    }
}

impl fmt::Display for ReviewStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewReadiness {
    pub review_id: String,
    pub session_id: String,
    pub plan_id: Option<String>,
    pub plan_revision: Option<i64>,
    pub plan_current: bool,
    pub source: ReviewSourceSnapshot,
    pub revision: i64,
    pub status: ReviewStatus,
    pub load_bearing_claims: Vec<String>,
    pub delegate_reports: Vec<ReviewDelegateReceipt>,
    pub issues: Vec<ReviewIssue>,
    pub blockers: Vec<ReviewBlocker>,
    pub receipt: Option<ReviewReceipt>,
    pub time_created: i64,
    pub time_updated: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewBlocker {
    pub claim_id: Option<String>,
    pub reason: String,
}

impl fmt::Display for ReviewBlocker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.claim_id.as_deref() {
            Some(claim) => write!(f, "claim `{claim}` {}", self.reason),
            None => f.write_str(&self.reason),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewFinalizeOutcome {
    pub readiness: ReviewReadiness,
    pub claims: Vec<ReviewClaim>,
    pub blockers: Vec<ReviewBlocker>,
}

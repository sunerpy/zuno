//! Durable, evidence-backed review coordination.
//!
//! Review is independent from Goal lifecycle. It records source snapshots,
//! delegated evidence, claim verification, and implementation readiness in the
//! existing per-session event log.

mod model;
mod report;
mod source;
mod store;
mod tools;

pub use model::{
    ActorRef, ClaimKind, ClaimPriority, ClaimStatus, CodeGraphIndexSnapshot, Countercheck,
    CountercheckKind, EvidenceAnchor, MAX_CLAIM_STATEMENT_CHARS, MAX_EVIDENCE_ANCHORS,
    MAX_LOAD_BEARING_CLAIMS, NewReviewClaim, ReviewBlocker, ReviewClaim, ReviewFinalizeOutcome,
    ReviewIssue, ReviewIssueKind, ReviewReadiness, ReviewSourceSnapshot, ReviewStatus, SystemLayer,
};
pub use report::{
    DEFAULT_MAX_CLAIMS_PER_REPORT, DelegationEvidenceReport, MAX_COUNTERCHECKS_PER_CLAIM,
    MAX_SUMMARY_BYTES, ReportLimits, ReportRejection, ReportedClaim, ReportedContradiction,
    ReportedUnresolved, seat_response_contract,
};
pub use source::{
    FixedReviewSourceProbe, RepositoryReviewSourceProbe, ReviewSourceProbe, SourceProbeError,
};
pub use store::{ReviewError, ReviewStore};
pub use tools::{
    REVIEW_CLAIM_DESCRIPTION, REVIEW_CLAIM_TOOL_ID, REVIEW_FINALIZE_DESCRIPTION,
    REVIEW_FINALIZE_TOOL_ID, REVIEW_GET_DESCRIPTION, REVIEW_GET_TOOL_ID, REVIEW_OPEN_DESCRIPTION,
    REVIEW_OPEN_TOOL_ID, ReviewClaimAction, ReviewClaimParams, ReviewClaimTool,
    ReviewFinalizeParams, ReviewFinalizeTool, ReviewGetParams, ReviewGetTool, ReviewOpenParams,
    ReviewOpenTool, review_tools,
};

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

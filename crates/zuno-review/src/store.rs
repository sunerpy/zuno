use crate::{
    ActorRef, ClaimKind, ClaimPriority, ClaimStatus, DelegationEvidenceReport, EvidenceAnchor,
    MAX_CLAIM_STATEMENT_CHARS, MAX_EVIDENCE_ANCHORS, MAX_LOAD_BEARING_CLAIMS, NewReviewClaim,
    ReviewBlocker, ReviewClaim, ReviewDelegateReceipt, ReviewFinalizeOutcome, ReviewIssue,
    ReviewIssueKind, ReviewReadiness, ReviewReceipt, ReviewSourceSnapshot, ReviewStatus,
    ReviewVerificationReceipt, delegation_report_digest,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use uuid::Uuid;
use zuno_db::event_log::{NewSessionEvent, SessionEvent, append_in, read_of_type_after_in};
use zuno_db::job::{AgentJobStore, JobStatus, JobSubject};
use zuno_db::{Pool, open};
use zuno_error::DbError;

const REVIEW_STARTED_EVENT: &str = "review.started";
const REVIEW_CLAIMS_EVENT: &str = "review.claim.recorded";
const REVIEW_CHANGED_EVENT: &str = "review.claim.changed";
const REVIEW_FINALIZED_EVENT: &str = "review.finalized";
const MAX_REVIEW_CLAIMS: usize = 64;
const MAX_REVIEW_ISSUES: usize = 12;
const MAX_ISSUE_TEXT_CHARS: usize = 500;
const MAX_BLOCKER_CHARS: usize = 500;

#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("review `{review_id}` does not exist for session `{session_id}`")]
    UnknownReview {
        session_id: String,
        review_id: String,
    },
    #[error("review claim `{claim_id}` does not exist in review `{review_id}`")]
    UnknownClaim { review_id: String, claim_id: String },
    #[error("review issue `{issue_id}` does not exist in review `{review_id}`")]
    UnknownIssue { review_id: String, issue_id: String },
    #[error(
        "review revision conflict for `{review_id}`: expected {expected}, current {actual}; \
         read the review again before changing it"
    )]
    RevisionConflict {
        review_id: String,
        expected: i64,
        actual: i64,
    },
    #[error("review field `{field}` must contain visible text")]
    EmptyField { field: &'static str },
    #[error("review claim statement is {actual} characters, exceeding the {max}-character limit")]
    StatementTooLong { actual: usize, max: usize },
    #[error("review claim cites {actual} anchors, exceeding the limit of {max}")]
    TooManyAnchors { actual: usize, max: usize },
    #[error("a `{kind}` review claim must cite at least one host-verified evidence anchor")]
    ClaimNeedsEvidence { kind: ClaimKind },
    #[error("review claim `{claim_id}` is missing counterchecks: {}", missing.join(", "))]
    MissingCounterchecks {
        claim_id: String,
        missing: Vec<String>,
    },
    #[error("review claim `{claim_id}` is stale and must be recorded against the current source")]
    StaleClaim { claim_id: String },
    #[error(
        "review claim `{claim_id}` cannot transition from `{status}` to `verified`; record a new claim"
    )]
    InvalidClaimTransition {
        claim_id: String,
        status: ClaimStatus,
    },
    #[error("review `{review_id}` source snapshot does not match report snapshot `{actual}`")]
    SourceMismatch { review_id: String, actual: String },
    #[error("review Council receipt is invalid: {0}")]
    InvalidCouncilReceipt(String),
    #[error("requested Plan `{id}` revision {revision} is not the current durable Plan")]
    PlanMismatch { id: String, revision: i64 },
    #[error("review report carries {actual} issues, exceeding the limit of {max}")]
    TooManyIssues { actual: usize, max: usize },
    #[error("review carries {actual} claims, exceeding the limit of {max}")]
    TooManyClaims { actual: usize, max: usize },
    #[error("review may rest on at most {max} load-bearing claims; received {actual}")]
    TooManyLoadBearingClaims { actual: usize, max: usize },
    #[error("review event `{event_type}` is corrupt: {detail}")]
    CorruptEvent { event_type: String, detail: String },
}

impl ReviewError {
    #[must_use]
    pub const fn is_model_correctable(&self) -> bool {
        !matches!(self, Self::Db(_) | Self::CorruptEvent { .. })
    }
}

#[derive(Clone)]
pub struct ReviewStore {
    pool: Arc<Pool>,
}

#[derive(Debug, Clone)]
pub(crate) struct ObservedClaimEvidence {
    pub claim_id: String,
    pub evidence: Result<Vec<EvidenceAnchor>, String>,
}

pub(crate) struct ReconcileObservation {
    pub current_source: ReviewSourceSnapshot,
    pub observed: Vec<ObservedClaimEvidence>,
    pub plan_current: bool,
}

pub(crate) struct FinalizeReview<'a> {
    pub session_id: &'a str,
    pub review_id: &'a str,
    pub expected_revision: i64,
    pub requested_status: ReviewStatus,
    pub commit_ready: bool,
    pub load_bearing_claim_ids: &'a [String],
    pub extra_blockers: &'a [ReviewBlocker],
    pub at_ms: i64,
}

pub(crate) struct FinalizeObservation {
    pub observed: Vec<ObservedClaimEvidence>,
    pub current_source: ReviewSourceSnapshot,
    pub plan_current: bool,
    pub source_changed_during_verification: bool,
}

#[derive(Debug, Clone)]
struct Projection {
    readiness: ReviewReadiness,
    claims: Vec<ReviewClaim>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StartedPayload {
    review_id: String,
    readiness: ReviewReadiness,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClaimsPayload {
    review_id: String,
    revision: i64,
    source: ReviewSourceSnapshot,
    plan_current: bool,
    claims: Vec<ReviewClaim>,
    delegate_reports: Vec<ReviewDelegateReceipt>,
    issues: Vec<ReviewIssue>,
    blockers: Vec<ReviewBlocker>,
    time_updated: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FinalizedPayload {
    review_id: String,
    readiness: ReviewReadiness,
}

impl ReviewStore {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    #[must_use]
    pub fn pool(&self) -> Arc<Pool> {
        Arc::clone(&self.pool)
    }

    pub fn open_review<F>(
        &self,
        session_id: &str,
        plan_id: Option<&str>,
        plan_revision: Option<i64>,
        plan_current: bool,
        capture_source: F,
        at_ms: i64,
    ) -> Result<ReviewReadiness, ReviewError>
    where
        F: FnOnce() -> Result<ReviewSourceSnapshot, ReviewError>,
    {
        let connection = self.pool.get()?;
        let transaction = open::immediate_transaction(&connection)?;
        let source = capture_source()?;
        let review_id = format!("rev_{}", Uuid::now_v7().simple());
        let plan_id = optional_visible(plan_id);
        let readiness = ReviewReadiness {
            review_id: review_id.clone(),
            session_id: session_id.to_owned(),
            plan_id,
            plan_revision,
            plan_current,
            source,
            revision: 1,
            status: ReviewStatus::Draft,
            load_bearing_claims: Vec::new(),
            delegate_reports: Vec::new(),
            issues: Vec::new(),
            blockers: Vec::new(),
            receipt: None,
            time_created: at_ms,
            time_updated: at_ms,
        };
        append_payload(
            &transaction,
            session_id,
            REVIEW_STARTED_EVENT,
            &StartedPayload {
                review_id,
                readiness: readiness.clone(),
            },
        )?;
        transaction.commit().map_err(open::map_error)?;
        Ok(readiness)
    }

    pub fn review(
        &self,
        session_id: &str,
        review_id: &str,
    ) -> Result<Option<ReviewReadiness>, ReviewError> {
        let connection = self.pool.get()?;
        Ok(projection_in(&connection, session_id, review_id)?.map(|value| value.readiness))
    }

    pub fn claims(
        &self,
        session_id: &str,
        review_id: &str,
    ) -> Result<Vec<ReviewClaim>, ReviewError> {
        let connection = self.pool.get()?;
        Ok(projection_in(&connection, session_id, review_id)?
            .map(|value| value.claims)
            .unwrap_or_default())
    }

    pub fn record_claim(
        &self,
        session_id: &str,
        expected_revision: i64,
        claim: NewReviewClaim,
        at_ms: i64,
    ) -> Result<(ReviewReadiness, ReviewClaim), ReviewError> {
        validate_new_claim(&claim)?;
        let connection = self.pool.get()?;
        let transaction = open::immediate_transaction(&connection)?;
        let mut projection = require_projection(&transaction, session_id, &claim.review_id)?;
        require_revision(&projection.readiness, expected_revision)?;
        if claim.source_snapshot_id != projection.readiness.source.id {
            return Err(ReviewError::SourceMismatch {
                review_id: claim.review_id,
                actual: claim.source_snapshot_id,
            });
        }
        if projection.claims.len() >= MAX_REVIEW_CLAIMS {
            return Err(ReviewError::TooManyClaims {
                actual: projection.claims.len() + 1,
                max: MAX_REVIEW_CLAIMS,
            });
        }
        projection.readiness.blockers.clear();
        let recorded = ReviewClaim {
            id: format!("rclaim_{}", Uuid::now_v7().simple()),
            review_id: projection.readiness.review_id.clone(),
            statement: claim.statement.trim().to_owned(),
            kind: claim.kind,
            priority: claim.priority,
            layer: claim.layer,
            source_snapshot_id: claim.source_snapshot_id,
            evidence: claim.evidence,
            counterchecks: claim.counterchecks,
            status: ClaimStatus::Unverified,
            asserted_by: claim.asserted_by,
            parent_verification: None,
            time_created: at_ms,
            time_updated: at_ms,
        };
        projection.claims.push(recorded.clone());
        append_claim_snapshot(
            &transaction,
            session_id,
            REVIEW_CLAIMS_EVENT,
            &mut projection,
            at_ms,
        )?;
        transaction.commit().map_err(open::map_error)?;
        Ok((projection.readiness, recorded))
    }

    pub fn import_report(
        &self,
        session_id: &str,
        review_id: &str,
        expected_revision: i64,
        receipt: ReviewDelegateReceipt,
        report: DelegationEvidenceReport,
        at_ms: i64,
    ) -> Result<ReviewReadiness, ReviewError> {
        validate_delegate_receipt(&receipt, &report)?;
        let connection = self.pool.get()?;
        let transaction = open::immediate_transaction(&connection)?;
        let mut projection = require_projection(&transaction, session_id, review_id)?;
        require_revision(&projection.readiness, expected_revision)?;
        if report.source_snapshot_id != projection.readiness.source.id {
            return Err(ReviewError::SourceMismatch {
                review_id: review_id.to_owned(),
                actual: report.source_snapshot_id,
            });
        }
        if let Some(existing) = projection
            .readiness
            .delegate_reports
            .iter()
            .find(|candidate| {
                candidate.run_id == receipt.run_id && candidate.seat_id == receipt.seat_id
            })
        {
            if same_delegate_receipt(existing, &receipt) {
                return Ok(projection.readiness);
            }
            return Err(ReviewError::InvalidCouncilReceipt(format!(
                "run `{}` seat `{}` was already imported with different authority",
                receipt.run_id, receipt.seat_id
            )));
        }
        if projection.claims.len().saturating_add(report.claims.len()) > MAX_REVIEW_CLAIMS {
            return Err(ReviewError::TooManyClaims {
                actual: projection.claims.len() + report.claims.len(),
                max: MAX_REVIEW_CLAIMS,
            });
        }
        projection.readiness.blockers.clear();
        for reported in report.claims {
            let new_claim = NewReviewClaim {
                review_id: review_id.to_owned(),
                statement: reported.statement,
                kind: reported.kind,
                priority: reported.priority,
                layer: reported.layer,
                source_snapshot_id: projection.readiness.source.id.clone(),
                evidence: reported.evidence,
                counterchecks: reported.counterchecks,
                asserted_by: ActorRef::Delegate(format!("{}:{}", receipt.run_id, receipt.seat_id)),
            };
            validate_new_claim(&new_claim)?;
            projection.claims.push(ReviewClaim {
                id: format!("rclaim_{}", Uuid::now_v7().simple()),
                review_id: review_id.to_owned(),
                statement: new_claim.statement.trim().to_owned(),
                kind: new_claim.kind,
                priority: new_claim.priority,
                layer: new_claim.layer,
                source_snapshot_id: new_claim.source_snapshot_id,
                evidence: new_claim.evidence,
                counterchecks: new_claim.counterchecks,
                status: ClaimStatus::Unverified,
                asserted_by: new_claim.asserted_by,
                parent_verification: None,
                time_created: at_ms,
                time_updated: at_ms,
            });
        }
        let incoming_issues = report
            .contradictions
            .len()
            .saturating_add(report.unresolved.len());
        if projection
            .readiness
            .issues
            .len()
            .saturating_add(incoming_issues)
            > MAX_REVIEW_ISSUES
        {
            return Err(ReviewError::TooManyIssues {
                actual: projection.readiness.issues.len() + incoming_issues,
                max: MAX_REVIEW_ISSUES,
            });
        }
        projection.readiness.issues.extend(
            report
                .contradictions
                .into_iter()
                .map(|issue| {
                    validate_issue_text(&issue.statement)?;
                    for detail in &issue.conflicting {
                        validate_issue_text(detail)?;
                    }
                    Ok(ReviewIssue {
                        id: format!("rissue_{}", Uuid::now_v7().simple()),
                        kind: ReviewIssueKind::Contradiction,
                        statement: issue.statement,
                        detail: issue.conflicting,
                        resolved: false,
                        resolution: None,
                    })
                })
                .collect::<Result<Vec<_>, ReviewError>>()?,
        );
        projection.readiness.delegate_reports.push(receipt);
        projection.readiness.issues.extend(
            report
                .unresolved
                .into_iter()
                .map(|issue| {
                    validate_issue_text(&issue.question)?;
                    if let Some(next) = issue.next_check.as_deref() {
                        validate_issue_text(next)?;
                    }
                    Ok(ReviewIssue {
                        id: format!("rissue_{}", Uuid::now_v7().simple()),
                        kind: ReviewIssueKind::Unresolved,
                        statement: issue.question,
                        detail: issue.next_check.into_iter().collect(),
                        resolved: false,
                        resolution: None,
                    })
                })
                .collect::<Result<Vec<_>, ReviewError>>()?,
        );
        append_claim_snapshot(
            &transaction,
            session_id,
            REVIEW_CLAIMS_EVENT,
            &mut projection,
            at_ms,
        )?;
        transaction.commit().map_err(open::map_error)?;
        Ok(projection.readiness)
    }

    pub fn verify_claim(
        &self,
        session_id: &str,
        review_id: &str,
        expected_revision: i64,
        claim_id: &str,
        verification: ReviewVerificationReceipt,
        at_ms: i64,
    ) -> Result<(ReviewReadiness, ReviewClaim), ReviewError> {
        self.change_claim(
            session_id,
            review_id,
            expected_revision,
            claim_id,
            at_ms,
            |claim| {
                if claim.status == ClaimStatus::Stale {
                    return Err(ReviewError::StaleClaim {
                        claim_id: claim.id.clone(),
                    });
                }
                if claim.status == ClaimStatus::Refuted {
                    return Err(ReviewError::InvalidClaimTransition {
                        claim_id: claim.id.clone(),
                        status: claim.status,
                    });
                }
                let missing = claim.missing_counterchecks();
                if !missing.is_empty() {
                    return Err(ReviewError::MissingCounterchecks {
                        claim_id: claim.id.clone(),
                        missing: missing.iter().map(ToString::to_string).collect(),
                    });
                }
                claim.status = ClaimStatus::Verified;
                claim.parent_verification = Some(verification);
                Ok(())
            },
        )
    }

    pub fn contest_claim(
        &self,
        session_id: &str,
        review_id: &str,
        expected_revision: i64,
        claim_id: &str,
        reason: &str,
        at_ms: i64,
    ) -> Result<(ReviewReadiness, ReviewClaim), ReviewError> {
        require_visible(reason, "contest reason")?;
        self.change_claim(
            session_id,
            review_id,
            expected_revision,
            claim_id,
            at_ms,
            |claim| {
                if claim.status != ClaimStatus::Refuted {
                    claim.status = ClaimStatus::Contested;
                    claim.parent_verification = None;
                }
                Ok(())
            },
        )
    }

    pub fn refute_claim(
        &self,
        session_id: &str,
        review_id: &str,
        expected_revision: i64,
        claim_id: &str,
        reason: &str,
        at_ms: i64,
    ) -> Result<(ReviewReadiness, ReviewClaim), ReviewError> {
        require_visible(reason, "refutation reason")?;
        self.change_claim(
            session_id,
            review_id,
            expected_revision,
            claim_id,
            at_ms,
            |claim| {
                claim.status = ClaimStatus::Refuted;
                claim.parent_verification = None;
                Ok(())
            },
        )
    }

    pub fn stale_claim(
        &self,
        session_id: &str,
        review_id: &str,
        expected_revision: i64,
        claim_id: &str,
        at_ms: i64,
    ) -> Result<(ReviewReadiness, ReviewClaim), ReviewError> {
        self.change_claim(
            session_id,
            review_id,
            expected_revision,
            claim_id,
            at_ms,
            |claim| {
                if claim.status != ClaimStatus::Refuted {
                    claim.status = ClaimStatus::Stale;
                    claim.parent_verification = None;
                }
                Ok(())
            },
        )
    }

    pub fn resolve_issue(
        &self,
        session_id: &str,
        review_id: &str,
        expected_revision: i64,
        issue_id: &str,
        resolution: &str,
        at_ms: i64,
    ) -> Result<ReviewReadiness, ReviewError> {
        let resolution = require_visible(resolution, "issue resolution")?;
        let connection = self.pool.get()?;
        let transaction = open::immediate_transaction(&connection)?;
        let mut projection = require_projection(&transaction, session_id, review_id)?;
        require_revision(&projection.readiness, expected_revision)?;
        let issue = projection
            .readiness
            .issues
            .iter_mut()
            .find(|issue| issue.id == issue_id)
            .ok_or_else(|| ReviewError::UnknownIssue {
                review_id: review_id.to_owned(),
                issue_id: issue_id.to_owned(),
            })?;
        issue.resolved = true;
        issue.resolution = Some(resolution);
        projection.readiness.blockers.clear();
        append_claim_snapshot(
            &transaction,
            session_id,
            REVIEW_CHANGED_EVENT,
            &mut projection,
            at_ms,
        )?;
        transaction.commit().map_err(open::map_error)?;
        Ok(projection.readiness)
    }

    pub fn record_system_blocker(
        &self,
        session_id: &str,
        review_id: &str,
        expected_revision: i64,
        reason: &str,
        at_ms: i64,
    ) -> Result<ReviewReadiness, ReviewError> {
        let reason = bounded_visible(reason, "system blocker", MAX_BLOCKER_CHARS)?;
        let connection = self.pool.get()?;
        let transaction = open::immediate_transaction(&connection)?;
        let mut projection = require_projection(&transaction, session_id, review_id)?;
        require_revision(&projection.readiness, expected_revision)?;
        projection.readiness.blockers = vec![ReviewBlocker {
            claim_id: None,
            reason,
        }];
        append_claim_snapshot(
            &transaction,
            session_id,
            REVIEW_CHANGED_EVENT,
            &mut projection,
            at_ms,
        )?;
        transaction.commit().map_err(open::map_error)?;
        Ok(projection.readiness)
    }

    pub(crate) fn reconcile_external<F>(
        &self,
        session_id: &str,
        review_id: &str,
        at_ms: i64,
        capture_source: F,
    ) -> Result<ReviewReadiness, ReviewError>
    where
        F: FnOnce(&ReviewReadiness, &[ReviewClaim]) -> Result<ReconcileObservation, ReviewError>,
    {
        let connection = self.pool.get()?;
        let transaction = open::immediate_transaction(&connection)?;
        let mut projection = require_projection(&transaction, session_id, review_id)?;
        let observation = capture_source(&projection.readiness, &projection.claims)?;
        let source_changed =
            !same_source(&projection.readiness.source, &observation.current_source);
        let plan_changed = projection.readiness.plan_current != observation.plan_current;
        let mut evidence_changed = false;
        for observed in &observation.observed {
            let Some(claim) = projection
                .claims
                .iter_mut()
                .find(|claim| claim.id == observed.claim_id)
            else {
                continue;
            };
            let matches = observed
                .evidence
                .as_ref()
                .is_ok_and(|anchors| same_evidence(&claim.evidence, anchors));
            if !matches && claim.status != ClaimStatus::Refuted {
                claim.status = ClaimStatus::Stale;
                claim.parent_verification = None;
                claim.time_updated = at_ms;
                evidence_changed = true;
            }
        }
        let receipt_changed = projection.readiness.status == ReviewStatus::Ready
            && !projection
                .readiness
                .receipt
                .as_ref()
                .is_some_and(|receipt| {
                    review_evidence_digest(&projection.claims)
                        .is_ok_and(|digest| receipt.evidence_digest == digest)
                });
        if !source_changed && !plan_changed && !evidence_changed && !receipt_changed {
            return Ok(projection.readiness);
        }
        let mut blockers = Vec::new();
        if source_changed {
            let previous = projection.readiness.source.worktree_digest.clone();
            projection.readiness.source = observation.current_source;
            for claim in &mut projection.claims {
                if claim.status != ClaimStatus::Refuted {
                    claim.status = ClaimStatus::Stale;
                    claim.parent_verification = None;
                    claim.time_updated = at_ms;
                }
            }
            blockers.push(ReviewBlocker {
                claim_id: None,
                reason: format!(
                    "review source changed from {previous} to {}",
                    projection.readiness.source.worktree_digest
                ),
            });
        }
        projection.readiness.plan_current = observation.plan_current;
        if !observation.plan_current {
            blockers.push(ReviewBlocker {
                claim_id: None,
                reason: "the Plan bound to this review is no longer the current revision"
                    .to_owned(),
            });
        }
        if evidence_changed {
            blockers.push(ReviewBlocker {
                claim_id: None,
                reason: "one or more review evidence anchors changed or became unreadable"
                    .to_owned(),
            });
        }
        if receipt_changed {
            blockers.push(ReviewBlocker {
                claim_id: None,
                reason: "the Ready receipt does not bind the current evidence anchors".to_owned(),
            });
        }
        projection.readiness.blockers = blockers;
        append_claim_snapshot(
            &transaction,
            session_id,
            REVIEW_CHANGED_EVENT,
            &mut projection,
            at_ms,
        )?;
        transaction.commit().map_err(open::map_error)?;
        Ok(projection.readiness)
    }

    pub(crate) fn finalize<F>(
        &self,
        request: FinalizeReview<'_>,
        observe: F,
    ) -> Result<ReviewFinalizeOutcome, ReviewError>
    where
        F: FnOnce(&ReviewReadiness, &[ReviewClaim]) -> Result<FinalizeObservation, ReviewError>,
    {
        if request.load_bearing_claim_ids.len() > MAX_LOAD_BEARING_CLAIMS {
            return Err(ReviewError::TooManyLoadBearingClaims {
                actual: request.load_bearing_claim_ids.len(),
                max: MAX_LOAD_BEARING_CLAIMS,
            });
        }
        let connection = self.pool.get()?;
        let transaction = open::immediate_transaction(&connection)?;
        let mut projection =
            require_projection(&transaction, request.session_id, request.review_id)?;
        require_revision(&projection.readiness, request.expected_revision)?;
        let observation = observe(&projection.readiness, &projection.claims)?;
        let source_changed =
            !same_source(&projection.readiness.source, &observation.current_source);
        if source_changed {
            projection.readiness.source = observation.current_source;
            for claim in &mut projection.claims {
                if claim.status != ClaimStatus::Refuted {
                    claim.status = ClaimStatus::Stale;
                    claim.parent_verification = None;
                    claim.time_updated = request.at_ms;
                }
            }
        }
        projection.readiness.plan_current = observation.plan_current;
        let mut changed = false;
        for observation in &observation.observed {
            let Some(claim) = projection
                .claims
                .iter_mut()
                .find(|claim| claim.id == observation.claim_id)
            else {
                continue;
            };
            let matches = observation
                .evidence
                .as_ref()
                .is_ok_and(|anchors| same_evidence(&claim.evidence, anchors));
            if !matches && claim.status != ClaimStatus::Refuted {
                claim.status = ClaimStatus::Stale;
                claim.parent_verification = None;
                claim.time_updated = request.at_ms;
                changed = true;
            }
        }
        if changed || source_changed {
            append_claim_snapshot(
                &transaction,
                request.session_id,
                REVIEW_CHANGED_EVENT,
                &mut projection,
                request.at_ms,
            )?;
        }
        let mut blockers = projection.readiness.blockers.clone();
        blockers.extend_from_slice(request.extra_blockers);
        if source_changed {
            blockers.push(ReviewBlocker {
                claim_id: None,
                reason: "the review source changed before finalization".to_owned(),
            });
        }
        if observation.source_changed_during_verification {
            blockers.push(ReviewBlocker {
                claim_id: None,
                reason: "the review source changed while evidence was being verified".to_owned(),
            });
        }
        blockers.extend(gate_blockers(
            &projection.readiness,
            &projection.claims,
            request.load_bearing_claim_ids,
            &AgentJobStore::new(Arc::clone(&self.pool)),
        )?);
        let status = if request.commit_ready
            && request.requested_status == ReviewStatus::Ready
            && blockers.is_empty()
        {
            ReviewStatus::Ready
        } else {
            ReviewStatus::Draft
        };
        projection.readiness.revision = projection
            .readiness
            .revision
            .checked_add(1)
            .ok_or_else(revision_exhausted)?;
        projection.readiness.status = status;
        projection.readiness.load_bearing_claims = request.load_bearing_claim_ids.to_vec();
        projection.readiness.blockers = blockers.clone();
        projection.readiness.receipt = if status == ReviewStatus::Ready {
            Some(ready_receipt(
                &projection.readiness,
                &projection.claims,
                request.at_ms,
            )?)
        } else {
            None
        };
        projection.readiness.time_updated = request.at_ms;
        append_payload(
            &transaction,
            request.session_id,
            REVIEW_FINALIZED_EVENT,
            &FinalizedPayload {
                review_id: request.review_id.to_owned(),
                readiness: projection.readiness.clone(),
            },
        )?;
        transaction.commit().map_err(open::map_error)?;
        Ok(ReviewFinalizeOutcome {
            readiness: projection.readiness,
            claims: projection.claims,
            blockers,
        })
    }

    pub(crate) fn promote_ready(
        &self,
        session_id: &str,
        review_id: &str,
        expected_revision: i64,
        at_ms: i64,
    ) -> Result<ReviewReadiness, ReviewError> {
        let connection = self.pool.get()?;
        let transaction = open::immediate_transaction(&connection)?;
        let mut projection = require_projection(&transaction, session_id, review_id)?;
        if projection.readiness.revision != expected_revision {
            return Ok(projection.readiness);
        }
        let blockers = gate_blockers(
            &projection.readiness,
            &projection.claims,
            &projection.readiness.load_bearing_claims,
            &AgentJobStore::new(Arc::clone(&self.pool)),
        )?;
        if !blockers.is_empty() {
            projection.readiness.blockers = blockers;
            append_claim_snapshot(
                &transaction,
                session_id,
                REVIEW_CHANGED_EVENT,
                &mut projection,
                at_ms,
            )?;
            transaction.commit().map_err(open::map_error)?;
            return Ok(projection.readiness);
        }
        projection.readiness.revision = projection
            .readiness
            .revision
            .checked_add(1)
            .ok_or_else(revision_exhausted)?;
        projection.readiness.status = ReviewStatus::Ready;
        projection.readiness.blockers.clear();
        projection.readiness.receipt = Some(ready_receipt(
            &projection.readiness,
            &projection.claims,
            at_ms,
        )?);
        projection.readiness.time_updated = at_ms;
        append_payload(
            &transaction,
            session_id,
            REVIEW_FINALIZED_EVENT,
            &FinalizedPayload {
                review_id: review_id.to_owned(),
                readiness: projection.readiness.clone(),
            },
        )?;
        transaction.commit().map_err(open::map_error)?;
        Ok(projection.readiness)
    }

    fn change_claim(
        &self,
        session_id: &str,
        review_id: &str,
        expected_revision: i64,
        claim_id: &str,
        at_ms: i64,
        change: impl FnOnce(&mut ReviewClaim) -> Result<(), ReviewError>,
    ) -> Result<(ReviewReadiness, ReviewClaim), ReviewError> {
        let connection = self.pool.get()?;
        let transaction = open::immediate_transaction(&connection)?;
        let mut projection = require_projection(&transaction, session_id, review_id)?;
        require_revision(&projection.readiness, expected_revision)?;
        let claim = projection
            .claims
            .iter_mut()
            .find(|claim| claim.id == claim_id)
            .ok_or_else(|| ReviewError::UnknownClaim {
                review_id: review_id.to_owned(),
                claim_id: claim_id.to_owned(),
            })?;
        projection.readiness.blockers.clear();
        change(claim)?;
        claim.time_updated = at_ms;
        let changed = claim.clone();
        append_claim_snapshot(
            &transaction,
            session_id,
            REVIEW_CHANGED_EVENT,
            &mut projection,
            at_ms,
        )?;
        transaction.commit().map_err(open::map_error)?;
        Ok((projection.readiness, changed))
    }
}

fn append_claim_snapshot(
    transaction: &zuno_db::Transaction<'_>,
    session_id: &str,
    event_type: &str,
    projection: &mut Projection,
    at_ms: i64,
) -> Result<(), ReviewError> {
    projection.readiness.revision = projection
        .readiness
        .revision
        .checked_add(1)
        .ok_or_else(revision_exhausted)?;
    projection.readiness.status = ReviewStatus::Draft;
    projection.readiness.receipt = None;
    projection.readiness.time_updated = at_ms;
    append_payload(
        transaction,
        session_id,
        event_type,
        &ClaimsPayload {
            review_id: projection.readiness.review_id.clone(),
            revision: projection.readiness.revision,
            source: projection.readiness.source.clone(),
            plan_current: projection.readiness.plan_current,
            claims: projection.claims.clone(),
            delegate_reports: projection.readiness.delegate_reports.clone(),
            issues: projection.readiness.issues.clone(),
            blockers: projection.readiness.blockers.clone(),
            time_updated: at_ms,
        },
    )?;
    Ok(())
}

fn projection_in(
    connection: &Connection,
    session_id: &str,
    review_id: &str,
) -> Result<Option<Projection>, ReviewError> {
    let mut events = Vec::new();
    for event_type in [
        REVIEW_STARTED_EVENT,
        REVIEW_CLAIMS_EVENT,
        REVIEW_CHANGED_EVENT,
        REVIEW_FINALIZED_EVENT,
    ] {
        events.extend(read_of_type_after_in(
            connection, session_id, event_type, None,
        )?);
    }
    events.sort_by_key(|event| event.sequence);
    let mut projection: Option<Projection> = None;
    for event in events {
        if event.properties.get("review_id").and_then(Value::as_str) != Some(review_id) {
            continue;
        }
        match event.event_type.as_str() {
            REVIEW_STARTED_EVENT => {
                let payload: StartedPayload = decode_payload(&event)?;
                projection = Some(Projection {
                    readiness: payload.readiness,
                    claims: Vec::new(),
                });
            }
            REVIEW_CLAIMS_EVENT | REVIEW_CHANGED_EVENT => {
                let payload: ClaimsPayload = decode_payload(&event)?;
                let current = projection
                    .as_mut()
                    .ok_or_else(|| corrupt(&event, "claim snapshot precedes review.started"))?;
                if payload.revision != current.readiness.revision + 1 {
                    return Err(corrupt(
                        &event,
                        &format!(
                            "revision {} does not follow {}",
                            payload.revision, current.readiness.revision
                        ),
                    ));
                }
                current.readiness.revision = payload.revision;
                current.readiness.status = ReviewStatus::Draft;
                current.readiness.receipt = None;
                current.readiness.source = payload.source;
                current.readiness.plan_current = payload.plan_current;
                current.readiness.delegate_reports = payload.delegate_reports;
                current.readiness.issues = payload.issues;
                current.readiness.blockers = payload.blockers;
                current.readiness.time_updated = payload.time_updated;
                current.claims = payload.claims;
            }
            REVIEW_FINALIZED_EVENT => {
                let payload: FinalizedPayload = decode_payload(&event)?;
                let current = projection
                    .as_mut()
                    .ok_or_else(|| corrupt(&event, "review.finalized precedes review.started"))?;
                if payload.readiness.revision != current.readiness.revision + 1 {
                    return Err(corrupt(
                        &event,
                        &format!(
                            "revision {} does not follow {}",
                            payload.readiness.revision, current.readiness.revision
                        ),
                    ));
                }
                current.readiness = payload.readiness;
            }
            _ => {}
        }
    }
    Ok(projection)
}

fn require_projection(
    connection: &Connection,
    session_id: &str,
    review_id: &str,
) -> Result<Projection, ReviewError> {
    projection_in(connection, session_id, review_id)?.ok_or_else(|| ReviewError::UnknownReview {
        session_id: session_id.to_owned(),
        review_id: review_id.to_owned(),
    })
}

fn require_revision(
    readiness: &ReviewReadiness,
    expected_revision: i64,
) -> Result<(), ReviewError> {
    if readiness.revision == expected_revision {
        Ok(())
    } else {
        Err(ReviewError::RevisionConflict {
            review_id: readiness.review_id.clone(),
            expected: expected_revision,
            actual: readiness.revision,
        })
    }
}

fn validate_new_claim(claim: &NewReviewClaim) -> Result<(), ReviewError> {
    require_visible(&claim.review_id, "review_id")?;
    let statement = require_visible(&claim.statement, "statement")?;
    if statement.chars().count() > MAX_CLAIM_STATEMENT_CHARS {
        return Err(ReviewError::StatementTooLong {
            actual: statement.chars().count(),
            max: MAX_CLAIM_STATEMENT_CHARS,
        });
    }
    if claim.evidence.len() > MAX_EVIDENCE_ANCHORS {
        return Err(ReviewError::TooManyAnchors {
            actual: claim.evidence.len(),
            max: MAX_EVIDENCE_ANCHORS,
        });
    }
    if claim.kind.requires_evidence() && claim.evidence.is_empty() {
        return Err(ReviewError::ClaimNeedsEvidence { kind: claim.kind });
    }
    for anchor in &claim.evidence {
        require_visible(&anchor.path, "evidence.path")?;
        require_visible(
            anchor.content_digest.as_deref().unwrap_or_default(),
            "evidence.content_digest",
        )?;
    }
    let mut kinds = BTreeSet::new();
    for countercheck in &claim.counterchecks {
        require_visible(&countercheck.detail, "countercheck.detail")?;
        if !kinds.insert(countercheck.kind) {
            return Err(ReviewError::CorruptEvent {
                event_type: REVIEW_CLAIMS_EVENT.to_owned(),
                detail: format!(
                    "countercheck `{}` appears more than once",
                    countercheck.kind
                ),
            });
        }
    }
    Ok(())
}

fn validate_issue_text(value: &str) -> Result<(), ReviewError> {
    let value = require_visible(value, "review issue")?;
    if value.chars().count() > MAX_ISSUE_TEXT_CHARS {
        return Err(ReviewError::StatementTooLong {
            actual: value.chars().count(),
            max: MAX_ISSUE_TEXT_CHARS,
        });
    }
    Ok(())
}

fn gate_blockers(
    readiness: &ReviewReadiness,
    claims: &[ReviewClaim],
    load_bearing_claim_ids: &[String],
    jobs: &AgentJobStore,
) -> Result<Vec<ReviewBlocker>, ReviewError> {
    let mut blockers = Vec::new();
    if !readiness.plan_current {
        blockers.push(ReviewBlocker {
            claim_id: None,
            reason: "the Plan bound to this review is no longer current".to_owned(),
        });
    }
    let mut runs: BTreeMap<(&str, &str, &str), BTreeSet<&str>> = BTreeMap::new();
    for receipt in &readiness.delegate_reports {
        let job_completed = receipt_job_completed(jobs, readiness, receipt)?;
        if receipt.preset == "balanced-review"
            && receipt.source_snapshot_id == readiness.source.id
            && job_completed
        {
            runs.entry((
                receipt.run_id.as_str(),
                receipt.job_id.as_str(),
                receipt.preset_source_id.as_str(),
            ))
            .or_default()
            .insert(receipt.seat_id.as_str());
        }
    }
    let validated_seats = runs.values().map(BTreeSet::len).max().unwrap_or_default();
    if validated_seats < 2 {
        blockers.push(ReviewBlocker {
            claim_id: None,
            reason: format!(
                "has {} validated Council seat report(s); balanced-review readiness requires at least 2",
                validated_seats
            ),
        });
    }
    if load_bearing_claim_ids.is_empty() {
        blockers.push(ReviewBlocker {
            claim_id: None,
            reason: "names no load-bearing claim".to_owned(),
        });
    }
    for claim_id in load_bearing_claim_ids {
        let claim = claims
            .iter()
            .find(|claim| &claim.id == claim_id)
            .ok_or_else(|| ReviewError::UnknownClaim {
                review_id: readiness.review_id.clone(),
                claim_id: claim_id.clone(),
            })?;
        if !claim.status.may_be_relied_on() {
            blockers.push(ReviewBlocker {
                claim_id: Some(claim.id.clone()),
                reason: format!("is `{}` rather than `verified`", claim.status),
            });
        }
        if !claim.parent_verification.as_ref().is_some_and(|receipt| {
            receipt.session_id == readiness.session_id && receipt.agent == "review"
        }) {
            blockers.push(ReviewBlocker {
                claim_id: Some(claim.id.clone()),
                reason: "was not re-verified by the review parent".to_owned(),
            });
        }
        if claim.kind == ClaimKind::Recommendation || claim.evidence.is_empty() {
            blockers.push(ReviewBlocker {
                claim_id: Some(claim.id.clone()),
                reason: "is not an evidence-bearing factual claim".to_owned(),
            });
        }
        let missing = claim.missing_counterchecks();
        if !missing.is_empty() {
            blockers.push(ReviewBlocker {
                claim_id: Some(claim.id.clone()),
                reason: format!(
                    "is missing counterchecks: {}",
                    missing
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }
        if claim.source_snapshot_id != readiness.source.id {
            blockers.push(ReviewBlocker {
                claim_id: Some(claim.id.clone()),
                reason: "is anchored to a superseded source snapshot".to_owned(),
            });
        }
    }
    for claim in claims {
        match claim.status {
            ClaimStatus::Contested => blockers.push(ReviewBlocker {
                claim_id: Some(claim.id.clone()),
                reason: "is contested".to_owned(),
            }),
            ClaimStatus::Stale => blockers.push(ReviewBlocker {
                claim_id: Some(claim.id.clone()),
                reason: "is stale".to_owned(),
            }),
            ClaimStatus::Unverified if claim.priority == ClaimPriority::P0 => {
                blockers.push(ReviewBlocker {
                    claim_id: Some(claim.id.clone()),
                    reason: "is a P0 claim that remains unverified".to_owned(),
                });
            }
            ClaimStatus::Unverified | ClaimStatus::Verified | ClaimStatus::Refuted => {}
        }
    }
    for issue in readiness.issues.iter().filter(|issue| !issue.resolved) {
        blockers.push(ReviewBlocker {
            claim_id: None,
            reason: format!(
                "review issue `{}` remains unresolved: {}",
                issue.id, issue.statement
            ),
        });
    }
    Ok(blockers)
}

fn receipt_job_completed(
    jobs: &AgentJobStore,
    readiness: &ReviewReadiness,
    receipt: &ReviewDelegateReceipt,
) -> Result<bool, ReviewError> {
    let job = match jobs.get(&receipt.job_id) {
        Ok(job) => job,
        Err(DbError::NotFound { .. }) => return Ok(false),
        Err(error) => return Err(ReviewError::Db(error)),
    };
    let subject_matches = matches!(
        &job.subject,
        JobSubject::Workflow { run_id, workflow }
            if run_id == &receipt.run_id
                && workflow == &format!("council:{}", receipt.preset)
    );
    let result_matches = job.result.as_ref().is_some_and(|result| {
        let run_matches =
            result.get("runID").and_then(Value::as_str) == Some(receipt.run_id.as_str());
        let preset_matches =
            result.get("preset").and_then(Value::as_str) == Some(receipt.preset.as_str());
        let completed = result.get("status").and_then(Value::as_str) == Some("completed");
        let seat_matches = result
            .get("seats")
            .and_then(Value::as_array)
            .is_some_and(|seats| {
                seats.iter().any(|seat| {
                    let report = seat.get("report").and_then(|report| {
                        serde_json::from_value::<DelegationEvidenceReport>(report.clone()).ok()
                    });
                    seat.get("id").and_then(Value::as_str) == Some(receipt.seat_id.as_str())
                        && seat.get("agent").and_then(Value::as_str) == Some(receipt.agent.as_str())
                        && seat.get("status").and_then(Value::as_str) == Some("completed")
                        && report.as_ref().is_some_and(|report| {
                            report.source_snapshot_id == receipt.source_snapshot_id
                                && delegation_report_digest(report) == receipt.report_digest
                        })
                })
            });
        run_matches && preset_matches && completed && seat_matches
    });
    Ok(job.parent_session_id == readiness.session_id
        && job.status == JobStatus::Completed
        && subject_matches
        && result_matches)
}

fn review_evidence_digest(claims: &[ReviewClaim]) -> Result<String, ReviewError> {
    #[derive(Serialize)]
    struct ClaimEvidence<'a> {
        claim_id: &'a str,
        evidence: &'a [EvidenceAnchor],
    }

    let payload = claims
        .iter()
        .map(|claim| ClaimEvidence {
            claim_id: &claim.id,
            evidence: &claim.evidence,
        })
        .collect::<Vec<_>>();
    let encoded = serde_json::to_vec(&payload).map_err(|error| ReviewError::CorruptEvent {
        event_type: REVIEW_FINALIZED_EVENT.to_owned(),
        detail: format!("evidence digest serialization failed: {error}"),
    })?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(encoded))))
}

fn ready_receipt(
    readiness: &ReviewReadiness,
    claims: &[ReviewClaim],
    at_ms: i64,
) -> Result<ReviewReceipt, ReviewError> {
    Ok(ReviewReceipt {
        id: format!("rrcp_{}", Uuid::now_v7().simple()),
        review_revision: readiness.revision,
        source_snapshot_id: readiness.source.id.clone(),
        head_sha: readiness.source.head_sha.clone(),
        worktree_digest: readiness.source.worktree_digest.clone(),
        artifact_digest: readiness
            .source
            .artifact
            .as_ref()
            .map(|artifact| artifact.content_digest.clone()),
        evidence_digest: review_evidence_digest(claims)?,
        time_issued: at_ms,
    })
}

fn same_evidence(expected: &[EvidenceAnchor], observed: &[EvidenceAnchor]) -> bool {
    expected.len() == observed.len()
        && expected.iter().zip(observed).all(|(expected, observed)| {
            expected.path == observed.path
                && expected.symbol == observed.symbol
                && expected.start_line == observed.start_line
                && expected.end_line == observed.end_line
                && expected.content_digest == observed.content_digest
        })
}

fn same_source(expected: &ReviewSourceSnapshot, observed: &ReviewSourceSnapshot) -> bool {
    expected.repository_root == observed.repository_root
        && expected.head_sha == observed.head_sha
        && expected.branch == observed.branch
        && expected.worktree_path == observed.worktree_path
        && expected.dirty == observed.dirty
        && expected.worktree_digest == observed.worktree_digest
        && expected.scope_paths == observed.scope_paths
        && expected.artifact == observed.artifact
        && expected.codegraph == observed.codegraph
}

fn validate_delegate_receipt(
    receipt: &ReviewDelegateReceipt,
    report: &DelegationEvidenceReport,
) -> Result<(), ReviewError> {
    for (field, value) in [
        ("run_id", receipt.run_id.as_str()),
        ("job_id", receipt.job_id.as_str()),
        ("preset", receipt.preset.as_str()),
        ("preset_source_id", receipt.preset_source_id.as_str()),
        ("seat_id", receipt.seat_id.as_str()),
        ("agent", receipt.agent.as_str()),
        ("source_snapshot_id", receipt.source_snapshot_id.as_str()),
        ("report_digest", receipt.report_digest.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(ReviewError::InvalidCouncilReceipt(format!(
                "`{field}` must contain visible text"
            )));
        }
    }
    if receipt.preset != "balanced-review" {
        return Err(ReviewError::InvalidCouncilReceipt(format!(
            "preset `{}` is not the required `balanced-review` preset",
            receipt.preset
        )));
    }
    if receipt.source_snapshot_id != report.source_snapshot_id {
        return Err(ReviewError::InvalidCouncilReceipt(
            "receipt and report name different source snapshots".to_owned(),
        ));
    }
    let expected = crate::delegation_report_digest(report);
    if receipt.report_digest != expected {
        return Err(ReviewError::InvalidCouncilReceipt(
            "report digest does not match the validated report".to_owned(),
        ));
    }
    Ok(())
}

fn same_delegate_receipt(
    expected: &ReviewDelegateReceipt,
    observed: &ReviewDelegateReceipt,
) -> bool {
    expected.run_id == observed.run_id
        && expected.job_id == observed.job_id
        && expected.preset == observed.preset
        && expected.preset_source_id == observed.preset_source_id
        && expected.seat_id == observed.seat_id
        && expected.agent == observed.agent
        && expected.source_snapshot_id == observed.source_snapshot_id
        && expected.report_digest == observed.report_digest
}

fn append_payload<T: Serialize>(
    transaction: &zuno_db::Transaction<'_>,
    session_id: &str,
    event_type: &str,
    payload: &T,
) -> Result<(), ReviewError> {
    let value = serde_json::to_value(payload).map_err(|source| ReviewError::CorruptEvent {
        event_type: event_type.to_owned(),
        detail: source.to_string(),
    })?;
    let Value::Object(properties) = value else {
        return Err(ReviewError::CorruptEvent {
            event_type: event_type.to_owned(),
            detail: "event payload is not an object".to_owned(),
        });
    };
    append_in(
        transaction,
        session_id,
        NewSessionEvent::new(event_type, properties)?,
    )?;
    Ok(())
}

fn decode_payload<T: for<'de> Deserialize<'de>>(event: &SessionEvent) -> Result<T, ReviewError> {
    serde_json::from_value(Value::Object(event.properties.clone())).map_err(|source| {
        ReviewError::CorruptEvent {
            event_type: event.event_type.clone(),
            detail: source.to_string(),
        }
    })
}

fn corrupt(event: &SessionEvent, detail: &str) -> ReviewError {
    ReviewError::CorruptEvent {
        event_type: event.event_type.clone(),
        detail: detail.to_owned(),
    }
}

fn require_visible(value: &str, field: &'static str) -> Result<String, ReviewError> {
    let value = value.trim();
    if value
        .chars()
        .any(|character| !character.is_whitespace() && character != '\u{200b}')
    {
        Ok(value.to_owned())
    } else {
        Err(ReviewError::EmptyField { field })
    }
}

fn bounded_visible(
    value: &str,
    field: &'static str,
    max_chars: usize,
) -> Result<String, ReviewError> {
    let value = require_visible(value, field)?;
    if value.chars().count() <= max_chars {
        return Ok(value);
    }
    let mut bounded = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    bounded.push('…');
    Ok(bounded)
}

fn optional_visible(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn revision_exhausted() -> ReviewError {
    ReviewError::Db(DbError::Query {
        source: Box::new(std::io::Error::other("review revision exhausted")),
    })
}

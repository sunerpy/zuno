use crate::{
    ActorRef, ClaimKind, ClaimPriority, ClaimStatus, DelegationEvidenceReport, EvidenceAnchor,
    MAX_CLAIM_STATEMENT_CHARS, MAX_EVIDENCE_ANCHORS, MAX_LOAD_BEARING_CLAIMS, NewReviewClaim,
    ReviewBlocker, ReviewClaim, ReviewFinalizeOutcome, ReviewIssue, ReviewIssueKind,
    ReviewReadiness, ReviewSourceSnapshot, ReviewStatus,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::sync::Arc;
use uuid::Uuid;
use zuno_db::event_log::{NewSessionEvent, SessionEvent, append_in, read_of_type_after_in};
use zuno_db::{Pool, open};
use zuno_error::DbError;

const REVIEW_STARTED_EVENT: &str = "review.started";
const REVIEW_CLAIMS_EVENT: &str = "review.claim.recorded";
const REVIEW_CHANGED_EVENT: &str = "review.claim.changed";
const REVIEW_FINALIZED_EVENT: &str = "review.finalized";
const MAX_REVIEW_CLAIMS: usize = 64;
const MAX_REVIEW_ISSUES: usize = 12;
const MAX_ISSUE_TEXT_CHARS: usize = 500;

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
    #[error("review `{review_id}` source snapshot does not match report snapshot `{actual}`")]
    SourceMismatch { review_id: String, actual: String },
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

pub(crate) struct FinalizeReview<'a> {
    pub session_id: &'a str,
    pub review_id: &'a str,
    pub expected_revision: i64,
    pub requested_status: ReviewStatus,
    pub load_bearing_claim_ids: &'a [String],
    pub extra_blockers: &'a [ReviewBlocker],
    pub observed: &'a [ObservedClaimEvidence],
    pub at_ms: i64,
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
    claims: Vec<ReviewClaim>,
    delegate_reports: Vec<String>,
    issues: Vec<ReviewIssue>,
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

    pub fn open_review(
        &self,
        session_id: &str,
        artifact_path: Option<&str>,
        plan_id: Option<&str>,
        plan_revision: Option<i64>,
        source: ReviewSourceSnapshot,
        at_ms: i64,
    ) -> Result<ReviewReadiness, ReviewError> {
        let review_id = format!("rev_{}", Uuid::now_v7().simple());
        let artifact_path = optional_visible(artifact_path);
        let plan_id = optional_visible(plan_id);
        let readiness = ReviewReadiness {
            review_id: review_id.clone(),
            session_id: session_id.to_owned(),
            artifact_path,
            plan_id,
            plan_revision,
            source,
            revision: 1,
            status: ReviewStatus::Draft,
            load_bearing_claims: Vec::new(),
            delegate_reports: Vec::new(),
            issues: Vec::new(),
            receipt_id: None,
            time_created: at_ms,
            time_updated: at_ms,
        };
        let connection = self.pool.get()?;
        let transaction = open::immediate_transaction(&connection)?;
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
            verified_by_parent: false,
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
        delegate_id: &str,
        report: DelegationEvidenceReport,
        at_ms: i64,
    ) -> Result<ReviewReadiness, ReviewError> {
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
        if projection.claims.len().saturating_add(report.claims.len()) > MAX_REVIEW_CLAIMS {
            return Err(ReviewError::TooManyClaims {
                actual: projection.claims.len() + report.claims.len(),
                max: MAX_REVIEW_CLAIMS,
            });
        }
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
                asserted_by: ActorRef::Delegate(delegate_id.to_owned()),
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
                verified_by_parent: false,
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
        if !projection
            .readiness
            .delegate_reports
            .iter()
            .any(|seat| seat == delegate_id)
        {
            projection
                .readiness
                .delegate_reports
                .push(delegate_id.to_owned());
        }
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
        by_parent: bool,
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
                let missing = claim.missing_counterchecks();
                if !missing.is_empty() {
                    return Err(ReviewError::MissingCounterchecks {
                        claim_id: claim.id.clone(),
                        missing: missing.iter().map(ToString::to_string).collect(),
                    });
                }
                claim.status = ClaimStatus::Verified;
                claim.verified_by_parent |= by_parent;
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
                    claim.verified_by_parent = false;
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
                claim.verified_by_parent = true;
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
                    claim.verified_by_parent = false;
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

    pub(crate) fn finalize(
        &self,
        request: FinalizeReview<'_>,
    ) -> Result<ReviewFinalizeOutcome, ReviewError> {
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
        let mut changed = false;
        for observation in request.observed {
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
                claim.verified_by_parent = false;
                claim.time_updated = request.at_ms;
                changed = true;
            }
        }
        if changed {
            append_claim_snapshot(
                &transaction,
                request.session_id,
                REVIEW_CHANGED_EVENT,
                &mut projection,
                request.at_ms,
            )?;
        }
        let mut blockers = request.extra_blockers.to_vec();
        blockers.extend(gate_blockers(
            &projection.readiness,
            &projection.claims,
            request.load_bearing_claim_ids,
        )?);
        let status = if request.requested_status == ReviewStatus::Ready && blockers.is_empty() {
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
        projection.readiness.receipt_id =
            (status == ReviewStatus::Ready).then(|| format!("rrcp_{}", Uuid::now_v7().simple()));
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
    projection.readiness.receipt_id = None;
    projection.readiness.time_updated = at_ms;
    append_payload(
        transaction,
        session_id,
        event_type,
        &ClaimsPayload {
            review_id: projection.readiness.review_id.clone(),
            revision: projection.readiness.revision,
            claims: projection.claims.clone(),
            delegate_reports: projection.readiness.delegate_reports.clone(),
            issues: projection.readiness.issues.clone(),
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
                current.readiness.receipt_id = None;
                current.readiness.delegate_reports = payload.delegate_reports;
                current.readiness.issues = payload.issues;
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
) -> Result<Vec<ReviewBlocker>, ReviewError> {
    let mut blockers = Vec::new();
    if readiness.delegate_reports.len() < 2 {
        blockers.push(ReviewBlocker {
            claim_id: None,
            reason: format!(
                "has {} validated Council seat report(s); balanced-review readiness requires at least 2",
                readiness.delegate_reports.len()
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
        if !claim.verified_by_parent {
            blockers.push(ReviewBlocker {
                claim_id: Some(claim.id.clone()),
                reason: "was not re-verified by the review parent".to_owned(),
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

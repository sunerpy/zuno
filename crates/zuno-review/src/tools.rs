use crate::store::{FinalizeObservation, FinalizeReview, ObservedClaimEvidence};
use crate::{
    ClaimKind, ClaimPriority, Countercheck, EvidenceAnchor, MAX_EVIDENCE_OBSERVATION_BYTES,
    NewReviewClaim, ReviewBlocker, ReviewClaim, ReviewCouncilRunner, ReviewError,
    ReviewFinalizeOutcome, ReviewOpenRequest, ReviewPlanBinding, ReviewPlanProbe, ReviewReadiness,
    ReviewService, ReviewSourceSnapshot, ReviewStatus, ReviewVerificationReceipt, SystemLayer,
};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use zuno_error::ToolError;
use zuno_tool::{Tool, ToolContext, ToolOutput, ToolReplayPolicy, TypedTool, erase};

pub const REVIEW_OPEN_TOOL_ID: &str = "review_open";
pub const REVIEW_CLAIM_TOOL_ID: &str = "review_claim";
pub const REVIEW_GET_TOOL_ID: &str = "review_get";
pub const REVIEW_FINALIZE_TOOL_ID: &str = "review_finalize";

pub const REVIEW_OPEN_DESCRIPTION: &str = include_str!("description/review-open.txt");
pub const REVIEW_CLAIM_DESCRIPTION: &str = include_str!("description/review-claim.txt");
pub const REVIEW_GET_DESCRIPTION: &str = include_str!("description/review-get.txt");
pub const REVIEW_FINALIZE_DESCRIPTION: &str = include_str!("description/review-finalize.txt");

const MAX_SCOPE_PATHS: usize = 32;
const MAX_GET_CLAIMS: usize = 16;
const MAX_GET_BYTES: usize = 8 * 1_024;
const MIN_GET_BYTES: usize = 512;
const MAX_BLOCKERS: usize = 16;
const MAX_BLOCKER_CHARS: usize = 500;
const MAX_TOOL_JSON_BYTES: usize = 8 * 1_024;
const MAX_IDENTIFIER_CHARS: usize = 128;
const MAX_ERROR_MESSAGE_CHARS: usize = 1_024;

pub fn review_tools(
    service: Arc<ReviewService>,
    plans: Arc<dyn ReviewPlanProbe>,
    council: Arc<dyn ReviewCouncilRunner>,
) -> Vec<Arc<dyn Tool>> {
    vec![
        erase(ReviewOpenTool::new(
            Arc::clone(&service),
            Arc::clone(&plans),
            council,
        )),
        erase(ReviewClaimTool::new(
            Arc::clone(&service),
            Arc::clone(&plans),
        )),
        erase(ReviewGetTool::new(Arc::clone(&service), Arc::clone(&plans))),
        erase(ReviewFinalizeTool::new(service, plans)),
    ]
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewOpenParams {
    #[serde(default)]
    pub artifact_path: Option<String>,
    #[serde(default)]
    pub scope_paths: Vec<String>,
    #[serde(default)]
    pub plan_id: Option<String>,
    #[serde(default)]
    pub plan_revision: Option<i64>,
}

#[derive(Clone)]
pub struct ReviewOpenTool {
    service: Arc<ReviewService>,
    plans: Arc<dyn ReviewPlanProbe>,
    council: Arc<dyn ReviewCouncilRunner>,
}

impl ReviewOpenTool {
    #[must_use]
    pub fn new(
        service: Arc<ReviewService>,
        plans: Arc<dyn ReviewPlanProbe>,
        council: Arc<dyn ReviewCouncilRunner>,
    ) -> Self {
        Self {
            service,
            plans,
            council,
        }
    }
}

#[async_trait]
impl TypedTool for ReviewOpenTool {
    type Params = ReviewOpenParams;

    fn id(&self) -> &str {
        REVIEW_OPEN_TOOL_ID
    }

    fn description(&self) -> &str {
        REVIEW_OPEN_DESCRIPTION
    }

    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Never
    }

    async fn run(
        &self,
        params: ReviewOpenParams,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        require_review_agent(REVIEW_OPEN_TOOL_ID, &ctx)?;
        validate_scope(&params.scope_paths)?;
        if params.plan_id.is_some() != params.plan_revision.is_some() {
            return Err(invalid(
                REVIEW_OPEN_TOOL_ID,
                "plan_id and plan_revision must be supplied together",
            ));
        }
        if params.plan_revision.is_some_and(|revision| revision <= 0) {
            return Err(invalid(
                REVIEW_OPEN_TOOL_ID,
                "plan_revision must be positive",
            ));
        }
        let service = Arc::clone(&self.service);
        let opening_service = Arc::clone(&service);
        let plans = Arc::clone(&self.plans);
        let council = Arc::clone(&self.council);
        let council_context = ctx.clone();
        let session_id = ctx.session_id;
        let at_ms = now_ms(REVIEW_OPEN_TOOL_ID)?;
        let readiness = tokio::task::spawn_blocking(move || {
            opening_service.open_review(
                ReviewOpenRequest {
                    session_id: &session_id,
                    artifact_path: params.artifact_path.as_deref(),
                    scope_paths: &params.scope_paths,
                    plan: params.plan_id.map(|id| ReviewPlanBinding {
                        id,
                        revision: params
                            .plan_revision
                            .expect("validated paired plan revision"),
                    }),
                    at_ms,
                },
                plans.as_ref(),
            )
        })
        .await
        .map_err(|error| failed(REVIEW_OPEN_TOOL_ID, error))?
        .map_err(|error| map_review_error(REVIEW_OPEN_TOOL_ID, error))?;
        let question = review_question(&readiness);
        let council_result = council.run(&readiness, question, council_context).await;
        let (readiness, council_value) = match council_result {
            Ok(value) => {
                let store = service.store();
                let session_id = readiness.session_id.clone();
                let review_id = readiness.review_id.clone();
                let readiness = tokio::task::spawn_blocking(move || {
                    store.review(&session_id, &review_id)?.ok_or_else(|| {
                        ReviewError::UnknownReview {
                            session_id,
                            review_id,
                        }
                    })
                })
                .await
                .map_err(|error| failed(REVIEW_OPEN_TOOL_ID, error))?
                .map_err(|error| map_review_error(REVIEW_OPEN_TOOL_ID, error))?;
                (readiness, json!({"status":"completed","result":value}))
            }
            Err(error) => {
                let store = service.store();
                let session_id = readiness.session_id.clone();
                let review_id = readiness.review_id.clone();
                let at_ms = now_ms(REVIEW_OPEN_TOOL_ID)?;
                let error_for_store = error.clone();
                let readiness = tokio::task::spawn_blocking(move || {
                    let current = store.review(&session_id, &review_id)?.ok_or_else(|| {
                        ReviewError::UnknownReview {
                            session_id: session_id.clone(),
                            review_id: review_id.clone(),
                        }
                    })?;
                    store.record_system_blocker(
                        &session_id,
                        &review_id,
                        current.revision,
                        &format!("automatic balanced-review Council failed: {error_for_store}"),
                        at_ms,
                    )
                })
                .await
                .map_err(|join| failed(REVIEW_OPEN_TOOL_ID, join))?
                .map_err(|store| map_review_error(REVIEW_OPEN_TOOL_ID, store))?;
                (readiness, json!({"status":"failed","error":error}))
            }
        };
        Ok(ToolOutput::text(
            format!("review {} opened", readiness.review_id),
            bounded_value(
                json!({
                    "review": readiness_json(&readiness),
                    "council": council_value,
                }),
                MAX_TOOL_JSON_BYTES,
                &readiness.review_id,
                readiness.revision,
            ),
        ))
    }
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewClaimAction {
    Record,
    Verify,
    Contest,
    Refute,
    ResolveIssue,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewClaimParams {
    pub action: ReviewClaimAction,
    pub review_id: String,
    pub expected_revision: i64,
    #[serde(default)]
    pub claim_id: Option<String>,
    #[serde(default)]
    pub issue_id: Option<String>,
    #[serde(default)]
    pub statement: Option<String>,
    #[serde(default)]
    pub kind: Option<ClaimKind>,
    #[serde(default)]
    pub priority: Option<ClaimPriority>,
    #[serde(default)]
    pub layer: Option<SystemLayer>,
    #[serde(default)]
    pub evidence: Vec<EvidenceAnchor>,
    #[serde(default)]
    pub counterchecks: Vec<Countercheck>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Clone)]
pub struct ReviewClaimTool {
    service: Arc<ReviewService>,
    plans: Arc<dyn ReviewPlanProbe>,
}

impl ReviewClaimTool {
    #[must_use]
    pub fn new(service: Arc<ReviewService>, plans: Arc<dyn ReviewPlanProbe>) -> Self {
        Self { service, plans }
    }
}

#[async_trait]
impl TypedTool for ReviewClaimTool {
    type Params = ReviewClaimParams;

    fn id(&self) -> &str {
        REVIEW_CLAIM_TOOL_ID
    }

    fn description(&self) -> &str {
        REVIEW_CLAIM_DESCRIPTION
    }

    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Never
    }

    async fn run(
        &self,
        params: ReviewClaimParams,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        require_review_agent(REVIEW_CLAIM_TOOL_ID, &ctx)?;
        let review_id = required_identifier(REVIEW_CLAIM_TOOL_ID, &params.review_id, "review_id")?;
        if params.expected_revision <= 0 {
            return Err(invalid(
                REVIEW_CLAIM_TOOL_ID,
                "expected_revision must be the positive revision from review_get",
            ));
        }
        let at_ms = now_ms(REVIEW_CLAIM_TOOL_ID)?;
        let service = Arc::clone(&self.service);
        let plans = Arc::clone(&self.plans);
        let verification = ReviewVerificationReceipt {
            session_id: ctx.session_id.clone(),
            message_id: ctx.message_id.clone(),
            call_id: ctx.call_id.clone(),
            agent: ctx.agent.clone(),
            time_verified: at_ms,
        };
        let session_id = ctx.session_id;
        let result = tokio::task::spawn_blocking(
            move || -> Result<(ReviewReadiness, Option<ReviewClaim>), ToolError> {
                let store = service.store();
                let source = service.source();
                let readiness = service
                    .reconcile(&session_id, &review_id, plans.as_ref(), at_ms)
                    .map_err(|error| map_review_error(REVIEW_CLAIM_TOOL_ID, error))?;
                match params.action {
                    ReviewClaimAction::Record => {
                        let statement =
                            required_option(REVIEW_CLAIM_TOOL_ID, &params.statement, "statement")?;
                        let kind = params.kind.ok_or_else(|| {
                            invalid(REVIEW_CLAIM_TOOL_ID, "kind is required for record")
                        })?;
                        let priority = params.priority.ok_or_else(|| {
                            invalid(REVIEW_CLAIM_TOOL_ID, "priority is required for record")
                        })?;
                        let evidence = source
                            .anchor_batch(&params.evidence, MAX_EVIDENCE_OBSERVATION_BYTES)
                            .map_err(|error| invalid(REVIEW_CLAIM_TOOL_ID, &error.to_string()))?;
                        let (readiness, claim) = store
                            .record_claim(
                                &session_id,
                                params.expected_revision,
                                NewReviewClaim {
                                    review_id: review_id.clone(),
                                    statement,
                                    kind,
                                    priority,
                                    layer: params.layer,
                                    source_snapshot_id: readiness.source.id,
                                    evidence,
                                    counterchecks: params.counterchecks,
                                    asserted_by: crate::ActorRef::Parent,
                                },
                                at_ms,
                            )
                            .map_err(|error| map_review_error(REVIEW_CLAIM_TOOL_ID, error))?;
                        Ok((readiness, Some(claim)))
                    }
                    ReviewClaimAction::Verify => {
                        let claim_id = required_identifier_option(
                            REVIEW_CLAIM_TOOL_ID,
                            &params.claim_id,
                            "claim_id",
                        )?;
                        let stored = store
                            .claims(&session_id, &review_id)
                            .map_err(|error| map_review_error(REVIEW_CLAIM_TOOL_ID, error))?
                            .into_iter()
                            .find(|claim| claim.id == claim_id)
                            .ok_or_else(|| {
                                invalid(
                                    REVIEW_CLAIM_TOOL_ID,
                                    "claim_id does not exist in this review",
                                )
                            })?;
                        let observed =
                            source.anchor_batch(&stored.evidence, MAX_EVIDENCE_OBSERVATION_BYTES);
                        if !observed
                            .as_ref()
                            .is_ok_and(|anchors| evidence_matches(&stored.evidence, anchors))
                        {
                            let (readiness, claim) = store
                                .stale_claim(
                                    &session_id,
                                    &review_id,
                                    params.expected_revision,
                                    &claim_id,
                                    at_ms,
                                )
                                .map_err(|error| map_review_error(REVIEW_CLAIM_TOOL_ID, error))?;
                            return Ok((readiness, Some(claim)));
                        }
                        let (readiness, claim) = store
                            .verify_claim(
                                &session_id,
                                &review_id,
                                params.expected_revision,
                                &claim_id,
                                verification,
                                at_ms,
                            )
                            .map_err(|error| map_review_error(REVIEW_CLAIM_TOOL_ID, error))?;
                        Ok((readiness, Some(claim)))
                    }
                    ReviewClaimAction::Contest | ReviewClaimAction::Refute => {
                        let claim_id = required_identifier_option(
                            REVIEW_CLAIM_TOOL_ID,
                            &params.claim_id,
                            "claim_id",
                        )?;
                        let reason =
                            required_option(REVIEW_CLAIM_TOOL_ID, &params.reason, "reason")?;
                        let changed = match params.action {
                            ReviewClaimAction::Contest => store.contest_claim(
                                &session_id,
                                &review_id,
                                params.expected_revision,
                                &claim_id,
                                &reason,
                                at_ms,
                            ),
                            ReviewClaimAction::Refute => store.refute_claim(
                                &session_id,
                                &review_id,
                                params.expected_revision,
                                &claim_id,
                                &reason,
                                at_ms,
                            ),
                            _ => unreachable!(),
                        }
                        .map_err(|error| map_review_error(REVIEW_CLAIM_TOOL_ID, error))?;
                        Ok((changed.0, Some(changed.1)))
                    }
                    ReviewClaimAction::ResolveIssue => {
                        let issue_id = required_identifier_option(
                            REVIEW_CLAIM_TOOL_ID,
                            &params.issue_id,
                            "issue_id",
                        )?;
                        let resolution =
                            required_option(REVIEW_CLAIM_TOOL_ID, &params.reason, "reason")?;
                        let readiness = store
                            .resolve_issue(
                                &session_id,
                                &review_id,
                                params.expected_revision,
                                &issue_id,
                                &resolution,
                                at_ms,
                            )
                            .map_err(|error| map_review_error(REVIEW_CLAIM_TOOL_ID, error))?;
                        Ok((readiness, None))
                    }
                }
            },
        )
        .await
        .map_err(|error| failed(REVIEW_CLAIM_TOOL_ID, error))??;
        Ok(ToolOutput::text(
            format!("review revision {}", result.0.revision),
            bounded_value(
                json!({
                    "review": readiness_json(&result.0),
                    "claim": result.1,
                }),
                MAX_TOOL_JSON_BYTES,
                &result.0.review_id,
                result.0.revision,
            ),
        ))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewGetParams {
    pub review_id: String,
    #[serde(default)]
    pub claim_ids: Vec<String>,
    #[serde(default)]
    pub max_bytes: Option<u32>,
}

#[derive(Clone)]
pub struct ReviewGetTool {
    service: Arc<ReviewService>,
    plans: Arc<dyn ReviewPlanProbe>,
}

impl ReviewGetTool {
    #[must_use]
    pub fn new(service: Arc<ReviewService>, plans: Arc<dyn ReviewPlanProbe>) -> Self {
        Self { service, plans }
    }
}

#[async_trait]
impl TypedTool for ReviewGetTool {
    type Params = ReviewGetParams;

    fn id(&self) -> &str {
        REVIEW_GET_TOOL_ID
    }

    fn description(&self) -> &str {
        REVIEW_GET_DESCRIPTION
    }

    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Safe
    }

    async fn run(
        &self,
        params: ReviewGetParams,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        require_review_agent(REVIEW_GET_TOOL_ID, &ctx)?;
        let review_id = required_identifier(REVIEW_GET_TOOL_ID, &params.review_id, "review_id")?;
        if params.claim_ids.len() > MAX_GET_CLAIMS {
            return Err(invalid(
                REVIEW_GET_TOOL_ID,
                &format!("claim_ids accepts at most {MAX_GET_CLAIMS} values"),
            ));
        }
        for (index, claim_id) in params.claim_ids.iter().enumerate() {
            if claim_id.trim().is_empty() || claim_id.chars().count() > MAX_IDENTIFIER_CHARS {
                return Err(invalid(
                    REVIEW_GET_TOOL_ID,
                    &format!(
                        "claim_ids[{}] must contain 1 to {MAX_IDENTIFIER_CHARS} characters",
                        index + 1
                    ),
                ));
            }
        }
        if params
            .max_bytes
            .is_some_and(|bytes| bytes > 0 && (bytes as usize) < MIN_GET_BYTES)
        {
            return Err(invalid(
                REVIEW_GET_TOOL_ID,
                &format!("max_bytes must be at least {MIN_GET_BYTES} when supplied"),
            ));
        }
        let service = Arc::clone(&self.service);
        let plans = Arc::clone(&self.plans);
        let session_id = ctx.session_id;
        let at_ms = now_ms(REVIEW_GET_TOOL_ID)?;
        let (readiness, claims) = tokio::task::spawn_blocking(move || {
            let store = service.store();
            let readiness = service.reconcile(&session_id, &review_id, plans.as_ref(), at_ms)?;
            let claims = store.claims(&session_id, &review_id)?;
            Ok::<_, ReviewError>((readiness, claims))
        })
        .await
        .map_err(|error| failed(REVIEW_GET_TOOL_ID, error))?
        .map_err(|error| map_review_error(REVIEW_GET_TOOL_ID, error))?;
        let unknown = params
            .claim_ids
            .iter()
            .filter(|id| !claims.iter().any(|claim| claim.id == id.trim()))
            .take(MAX_GET_CLAIMS)
            .cloned()
            .collect::<Vec<_>>();
        if !unknown.is_empty() {
            return Err(invalid(
                REVIEW_GET_TOOL_ID,
                &format!("unknown claim_ids: {}", unknown.join(", ")),
            ));
        }
        let selected = select_claims(&claims, &params.claim_ids);
        let max_bytes = params
            .max_bytes
            .filter(|bytes| *bytes > 0)
            .map_or(MAX_GET_BYTES, |bytes| bytes as usize)
            .min(MAX_GET_BYTES);
        let output = bounded_review_json(&readiness, selected, max_bytes);
        Ok(ToolOutput::text(
            format!(
                "review {} revision {}",
                readiness.review_id, readiness.revision
            ),
            output,
        ))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewFinalizeParams {
    pub review_id: String,
    pub expected_revision: i64,
    pub requested_status: ReviewStatus,
    #[serde(default)]
    pub load_bearing_claim_ids: Vec<String>,
    #[serde(default)]
    pub blockers: Vec<String>,
}

#[derive(Clone)]
pub struct ReviewFinalizeTool {
    service: Arc<ReviewService>,
    plans: Arc<dyn ReviewPlanProbe>,
}

impl ReviewFinalizeTool {
    #[must_use]
    pub fn new(service: Arc<ReviewService>, plans: Arc<dyn ReviewPlanProbe>) -> Self {
        Self { service, plans }
    }
}

#[async_trait]
impl TypedTool for ReviewFinalizeTool {
    type Params = ReviewFinalizeParams;

    fn id(&self) -> &str {
        REVIEW_FINALIZE_TOOL_ID
    }

    fn description(&self) -> &str {
        REVIEW_FINALIZE_DESCRIPTION
    }

    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Never
    }

    async fn run(
        &self,
        params: ReviewFinalizeParams,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        require_review_agent(REVIEW_FINALIZE_TOOL_ID, &ctx)?;
        let review_id =
            required_identifier(REVIEW_FINALIZE_TOOL_ID, &params.review_id, "review_id")?;
        if params.expected_revision <= 0 {
            return Err(invalid(
                REVIEW_FINALIZE_TOOL_ID,
                "expected_revision must be positive",
            ));
        }
        if params.blockers.len() > MAX_BLOCKERS {
            return Err(invalid(
                REVIEW_FINALIZE_TOOL_ID,
                &format!("blockers accepts at most {MAX_BLOCKERS} values"),
            ));
        }
        if params.load_bearing_claim_ids.len() > crate::MAX_LOAD_BEARING_CLAIMS {
            return Err(invalid(
                REVIEW_FINALIZE_TOOL_ID,
                &format!(
                    "load_bearing_claim_ids accepts at most {} values",
                    crate::MAX_LOAD_BEARING_CLAIMS
                ),
            ));
        }
        for (index, claim_id) in params.load_bearing_claim_ids.iter().enumerate() {
            required_identifier(
                REVIEW_FINALIZE_TOOL_ID,
                claim_id,
                &format!("load_bearing_claim_ids[{}]", index + 1),
            )?;
        }
        let blockers = params
            .blockers
            .iter()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(|value| {
                if value.chars().count() > MAX_BLOCKER_CHARS {
                    Err(invalid(
                        REVIEW_FINALIZE_TOOL_ID,
                        &format!("one blocker exceeds {MAX_BLOCKER_CHARS} characters"),
                    ))
                } else {
                    Ok(ReviewBlocker {
                        claim_id: None,
                        reason: value.to_owned(),
                    })
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let service = Arc::clone(&self.service);
        let plans = Arc::clone(&self.plans);
        let session_id = ctx.session_id;
        let at_ms = now_ms(REVIEW_FINALIZE_TOOL_ID)?;
        let outcome = tokio::task::spawn_blocking(move || {
            let store = service.store();
            let prepared = store.finalize(
                FinalizeReview {
                    session_id: &session_id,
                    review_id: &review_id,
                    expected_revision: params.expected_revision,
                    requested_status: params.requested_status,
                    commit_ready: false,
                    load_bearing_claim_ids: &params.load_bearing_claim_ids,
                    extra_blockers: &blockers,
                    at_ms,
                },
                |readiness, claims| {
                    let plan_current = bound_plan_is_current(&plans, &session_id, readiness)
                        .map_err(|detail| ReviewError::CorruptEvent {
                            event_type: "review.plan.probe".to_owned(),
                            detail,
                        })?;
                    let artifact = readiness
                        .source
                        .artifact
                        .as_ref()
                        .map(|artifact| artifact.path.as_str());
                    let first_source = service
                        .capture(&readiness.source.scope_paths, artifact, at_ms)
                        .map_err(|error| ReviewError::CorruptEvent {
                            event_type: "review.source.probe".to_owned(),
                            detail: error.to_string(),
                        })?;
                    let first_observed = service.observe_claims(claims);
                    let confirmed_source = service
                        .capture(&readiness.source.scope_paths, artifact, at_ms)
                        .map_err(|error| ReviewError::CorruptEvent {
                            event_type: "review.source.probe".to_owned(),
                            detail: error.to_string(),
                        })?;
                    let confirmed_observed = service.observe_claims(claims);
                    Ok(FinalizeObservation {
                        observed: confirmed_observed.clone(),
                        current_source: confirmed_source.clone(),
                        plan_current,
                        source_changed_during_verification: !same_source_snapshot(
                            &first_source,
                            &confirmed_source,
                        ) || !same_observations(
                            &first_observed,
                            &confirmed_observed,
                        ),
                    })
                },
            )?;
            let readiness = service
                .reconcile(&session_id, &review_id, plans.as_ref(), at_ms)
                .map_err(|_| ReviewError::CorruptEvent {
                    event_type: "review.post_finalize.verify".to_owned(),
                    detail: "post-finalization evidence verification did not complete; the review remains draft"
                        .to_owned(),
                })?;
            let readiness = if params.requested_status == ReviewStatus::Ready
                && prepared.blockers.is_empty()
                && readiness.revision == prepared.readiness.revision
                && readiness.blockers.is_empty()
            {
                store.promote_ready(&session_id, &review_id, readiness.revision, at_ms)?
            } else {
                readiness
            };
            let claims = store.claims(&session_id, &review_id)?;
            let blockers = readiness.blockers.clone();
            Ok(ReviewFinalizeOutcome {
                readiness,
                claims,
                blockers,
            })
        })
        .await
        .map_err(|error| failed(REVIEW_FINALIZE_TOOL_ID, error))?
        .map_err(|error| map_review_error(REVIEW_FINALIZE_TOOL_ID, error))?;
        Ok(ToolOutput::text(
            format!("review {}", outcome.readiness.status),
            bounded_value(
                finalize_value(&outcome),
                MAX_TOOL_JSON_BYTES,
                &outcome.readiness.review_id,
                outcome.readiness.revision,
            ),
        ))
    }
}

fn select_claims(claims: &[ReviewClaim], ids: &[String]) -> Vec<ReviewClaim> {
    if ids.is_empty() {
        return claims.to_vec();
    }
    ids.iter()
        .filter_map(|id| claims.iter().find(|claim| claim.id == id.trim()).cloned())
        .collect()
}

fn bounded_review_json(
    readiness: &ReviewReadiness,
    mut claims: Vec<ReviewClaim>,
    max_bytes: usize,
) -> String {
    let selected_total = claims.len();
    loop {
        let value = json!({
            "review": readiness_json(readiness),
            "claims": claims,
            "omittedClaims": selected_total.saturating_sub(claims.len()),
        });
        let encoded = value.to_string();
        if encoded.len() <= max_bytes {
            return encoded;
        }
        if claims.pop().is_none() {
            return json!({
                "reviewID": readiness.review_id,
                "revision": readiness.revision,
                "status": readiness.status,
                "error": "review projection exceeds the requested byte limit",
            })
            .to_string();
        }
    }
}

fn readiness_json(readiness: &ReviewReadiness) -> Value {
    json!({
        "reviewID": readiness.review_id,
        "revision": readiness.revision,
        "status": readiness.status,
        "artifact": readiness.source.artifact,
        "planID": readiness.plan_id,
        "planRevision": readiness.plan_revision,
        "planCurrent": readiness.plan_current,
        "source": {
            "snapshotID": readiness.source.id,
            "headSHA": readiness.source.head_sha,
            "branch": readiness.source.branch,
            "dirty": readiness.source.dirty,
            "worktreeDigest": readiness.source.worktree_digest,
            "scopePaths": readiness.source.scope_paths,
            "codegraphTrustworthy": readiness.source.codegraph.is_trustworthy(),
        },
        "loadBearingClaimIDs": readiness.load_bearing_claims,
        "delegateReports": readiness.delegate_reports,
        "issues": readiness.issues,
        "blockers": readiness.blockers,
        "receipt": readiness.receipt,
    })
}

fn finalize_value(outcome: &ReviewFinalizeOutcome) -> Value {
    json!({
        "review": {
            "reviewID": outcome.readiness.review_id,
            "revision": outcome.readiness.revision,
            "status": outcome.readiness.status,
            "sourceSnapshotID": outcome.readiness.source.id,
            "loadBearingClaimIDs": outcome.readiness.load_bearing_claims,
            "delegateReports": outcome.readiness.delegate_reports,
            "openIssueCount": outcome
                .readiness
                .issues
                .iter()
                .filter(|issue| !issue.resolved)
                .count(),
            "receipt": outcome.readiness.receipt,
        },
        "blockers": outcome.blockers,
    })
}

fn validate_scope(scope: &[String]) -> Result<(), ToolError> {
    if scope.len() > MAX_SCOPE_PATHS {
        return Err(invalid(
            REVIEW_OPEN_TOOL_ID,
            &format!("scope_paths accepts at most {MAX_SCOPE_PATHS} values"),
        ));
    }
    if scope.iter().any(|path| path.trim().is_empty()) {
        return Err(invalid(
            REVIEW_OPEN_TOOL_ID,
            "scope_paths must not contain blank entries",
        ));
    }
    Ok(())
}

fn evidence_matches(expected: &[EvidenceAnchor], observed: &[EvidenceAnchor]) -> bool {
    expected.len() == observed.len()
        && expected.iter().zip(observed).all(|(left, right)| {
            left.path == right.path
                && left.symbol == right.symbol
                && left.start_line == right.start_line
                && left.end_line == right.end_line
                && left.content_digest == right.content_digest
        })
}

fn require_review_agent(tool: &str, ctx: &ToolContext) -> Result<(), ToolError> {
    if ctx.agent == "review" {
        Ok(())
    } else {
        Err(ToolError::Denied {
            tool: tool.to_owned(),
        })
    }
}

fn requested_plan_binding(
    plan_id: Option<&str>,
    plan_revision: Option<i64>,
) -> Option<ReviewPlanBinding> {
    match (
        plan_id.map(str::trim).filter(|value| !value.is_empty()),
        plan_revision,
    ) {
        (Some(id), Some(revision)) => Some(ReviewPlanBinding {
            id: id.to_owned(),
            revision,
        }),
        _ => None,
    }
}

fn bound_plan_is_current(
    plans: &Arc<dyn ReviewPlanProbe>,
    session_id: &str,
    readiness: &ReviewReadiness,
) -> Result<bool, String> {
    let Some(expected) =
        requested_plan_binding(readiness.plan_id.as_deref(), readiness.plan_revision)
    else {
        return Ok(true);
    };
    Ok(plans.current(session_id)?.as_ref() == Some(&expected))
}

fn same_source_snapshot(left: &ReviewSourceSnapshot, right: &ReviewSourceSnapshot) -> bool {
    left.repository_root == right.repository_root
        && left.head_sha == right.head_sha
        && left.branch == right.branch
        && left.worktree_path == right.worktree_path
        && left.dirty == right.dirty
        && left.worktree_digest == right.worktree_digest
        && left.scope_paths == right.scope_paths
        && left.artifact == right.artifact
        && left.codegraph == right.codegraph
}

fn same_observations(left: &[ObservedClaimEvidence], right: &[ObservedClaimEvidence]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.claim_id == right.claim_id
                && match (&left.evidence, &right.evidence) {
                    (Ok(left), Ok(right)) => evidence_matches(left, right),
                    (Err(left), Err(right)) => left == right,
                    (Ok(_), Err(_)) | (Err(_), Ok(_)) => false,
                }
        })
}

fn review_question(readiness: &ReviewReadiness) -> String {
    let artifact = readiness
        .source
        .artifact
        .as_ref()
        .map_or("the requested design or plan", |artifact| {
            artifact.path.as_str()
        });
    let scope = if readiness.source.scope_paths.is_empty() {
        "the repository".to_owned()
    } else {
        readiness.source.scope_paths.join(", ")
    };
    format!(
        "Review {artifact} against the current implementation in {scope}. Identify blockers, \
         counterexamples, persistence and lifecycle gaps, test omissions, and whether the artifact \
         is ready to implement."
    )
}

fn bounded_value(value: Value, max_bytes: usize, review_id: &str, revision: i64) -> String {
    let encoded = value.to_string();
    if encoded.len() <= max_bytes {
        return encoded;
    }
    let fallback = json!({
        "reviewID": review_id,
        "revision": revision,
        "omitted": true,
        "reason": "review tool output exceeded its byte limit",
    })
    .to_string();
    if fallback.len() <= max_bytes {
        fallback
    } else {
        "{\"omitted\":true}".to_owned()
    }
}

fn required_text(tool: &str, value: &str, field: &str) -> Result<String, ToolError> {
    let value = value.trim();
    if value.is_empty() {
        Err(invalid(tool, &format!("{field} must contain visible text")))
    } else {
        Ok(value.to_owned())
    }
}

fn required_identifier(tool: &str, value: &str, field: &str) -> Result<String, ToolError> {
    let value = required_text(tool, value, field)?;
    if value.chars().count() > MAX_IDENTIFIER_CHARS {
        return Err(invalid(
            tool,
            &format!("{field} exceeds {MAX_IDENTIFIER_CHARS} characters"),
        ));
    }
    Ok(value)
}

fn required_option(tool: &str, value: &Option<String>, field: &str) -> Result<String, ToolError> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| invalid(tool, &format!("{field} is required for this action")))
}

fn required_identifier_option(
    tool: &str,
    value: &Option<String>,
    field: &str,
) -> Result<String, ToolError> {
    let value = required_option(tool, value, field)?;
    required_identifier(tool, &value, field)
}

fn map_review_error(tool: &str, error: ReviewError) -> ToolError {
    if error.is_model_correctable() {
        invalid(tool, &error.to_string())
    } else {
        failed(tool, error)
    }
}

fn invalid(tool: &str, message: &str) -> ToolError {
    ToolError::InvalidArgs {
        tool: tool.to_owned(),
        source: Box::new(std::io::Error::other(bounded_message(message))),
    }
}

fn failed(tool: &str, error: impl std::fmt::Display) -> ToolError {
    ToolError::Failed {
        tool: tool.to_owned(),
        source: Box::new(std::io::Error::other(bounded_message(&error.to_string()))),
    }
}

fn bounded_message(message: &str) -> String {
    if message.chars().count() <= MAX_ERROR_MESSAGE_CHARS {
        return message.to_owned();
    }
    let mut bounded = message
        .chars()
        .take(MAX_ERROR_MESSAGE_CHARS.saturating_sub(1))
        .collect::<String>();
    bounded.push('…');
    bounded
}

fn now_ms(tool: &str) -> Result<i64, ToolError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| failed(tool, error))?;
    i64::try_from(elapsed.as_millis()).map_err(|error| failed(tool, error))
}

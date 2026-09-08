use crate::store::{FinalizeReview, ObservedClaimEvidence};
use crate::{
    ClaimKind, ClaimPriority, Countercheck, EvidenceAnchor, NewReviewClaim, ReviewBlocker,
    ReviewClaim, ReviewError, ReviewFinalizeOutcome, ReviewReadiness, ReviewSourceProbe,
    ReviewStatus, ReviewStore, SystemLayer,
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

pub fn review_tools(
    store: Arc<ReviewStore>,
    source: Arc<dyn ReviewSourceProbe>,
) -> Vec<Arc<dyn Tool>> {
    vec![
        erase(ReviewOpenTool::new(Arc::clone(&store), Arc::clone(&source))),
        erase(ReviewClaimTool::new(
            Arc::clone(&store),
            Arc::clone(&source),
        )),
        erase(ReviewGetTool::new(Arc::clone(&store))),
        erase(ReviewFinalizeTool::new(store, source)),
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
    store: Arc<ReviewStore>,
    source: Arc<dyn ReviewSourceProbe>,
}

impl ReviewOpenTool {
    #[must_use]
    pub fn new(store: Arc<ReviewStore>, source: Arc<dyn ReviewSourceProbe>) -> Self {
        Self { store, source }
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
        let store = Arc::clone(&self.store);
        let source = Arc::clone(&self.source);
        let session_id = ctx.session_id;
        let at_ms = now_ms(REVIEW_OPEN_TOOL_ID)?;
        let readiness = tokio::task::spawn_blocking(move || {
            let snapshot = source
                .capture(&params.scope_paths, at_ms)
                .map_err(|error| ReviewError::CorruptEvent {
                    event_type: "review.source.probe".to_owned(),
                    detail: error.to_string(),
                })?;
            store.open_review(
                &session_id,
                params.artifact_path.as_deref(),
                params.plan_id.as_deref(),
                params.plan_revision,
                snapshot,
                at_ms,
            )
        })
        .await
        .map_err(|error| failed(REVIEW_OPEN_TOOL_ID, error))?
        .map_err(|error| map_review_error(REVIEW_OPEN_TOOL_ID, error))?;
        Ok(ToolOutput::text(
            format!("review {} opened", readiness.review_id),
            readiness_json(&readiness).to_string(),
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
    store: Arc<ReviewStore>,
    source: Arc<dyn ReviewSourceProbe>,
}

impl ReviewClaimTool {
    #[must_use]
    pub fn new(store: Arc<ReviewStore>, source: Arc<dyn ReviewSourceProbe>) -> Self {
        Self { store, source }
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
        let review_id = required_text(REVIEW_CLAIM_TOOL_ID, &params.review_id, "review_id")?;
        if params.expected_revision <= 0 {
            return Err(invalid(
                REVIEW_CLAIM_TOOL_ID,
                "expected_revision must be the positive revision from review_get",
            ));
        }
        let at_ms = now_ms(REVIEW_CLAIM_TOOL_ID)?;
        let store = Arc::clone(&self.store);
        let source = Arc::clone(&self.source);
        let session_id = ctx.session_id;
        let parent_agent = ctx.agent == "review";
        let result = tokio::task::spawn_blocking(
            move || -> Result<(ReviewReadiness, Option<ReviewClaim>), ToolError> {
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
                        let readiness = store
                            .review(&session_id, &review_id)
                            .map_err(|error| map_review_error(REVIEW_CLAIM_TOOL_ID, error))?
                            .ok_or_else(|| {
                                invalid(REVIEW_CLAIM_TOOL_ID, "review_id does not exist")
                            })?;
                        let evidence = params
                            .evidence
                            .iter()
                            .map(|anchor| source.anchor(anchor))
                            .collect::<Result<Vec<_>, _>>()
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
                        let claim_id =
                            required_option(REVIEW_CLAIM_TOOL_ID, &params.claim_id, "claim_id")?;
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
                        let observed = stored
                            .evidence
                            .iter()
                            .map(|anchor| source.anchor(anchor))
                            .collect::<Result<Vec<_>, _>>();
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
                                parent_agent,
                                at_ms,
                            )
                            .map_err(|error| map_review_error(REVIEW_CLAIM_TOOL_ID, error))?;
                        Ok((readiness, Some(claim)))
                    }
                    ReviewClaimAction::Contest | ReviewClaimAction::Refute => {
                        let claim_id =
                            required_option(REVIEW_CLAIM_TOOL_ID, &params.claim_id, "claim_id")?;
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
                        let issue_id =
                            required_option(REVIEW_CLAIM_TOOL_ID, &params.issue_id, "issue_id")?;
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
            json!({
                "review": readiness_json(&result.0),
                "claim": result.1,
            })
            .to_string(),
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
    store: Arc<ReviewStore>,
}

impl ReviewGetTool {
    #[must_use]
    pub fn new(store: Arc<ReviewStore>) -> Self {
        Self { store }
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
        let review_id = required_text(REVIEW_GET_TOOL_ID, &params.review_id, "review_id")?;
        if params.claim_ids.len() > MAX_GET_CLAIMS {
            return Err(invalid(
                REVIEW_GET_TOOL_ID,
                &format!("claim_ids accepts at most {MAX_GET_CLAIMS} values"),
            ));
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
        let store = Arc::clone(&self.store);
        let session_id = ctx.session_id;
        let (readiness, claims) = tokio::task::spawn_blocking(move || {
            let readiness = store.review(&session_id, &review_id)?.ok_or_else(|| {
                ReviewError::UnknownReview {
                    session_id: session_id.clone(),
                    review_id: review_id.clone(),
                }
            })?;
            let claims = store.claims(&session_id, &review_id)?;
            Ok::<_, ReviewError>((readiness, claims))
        })
        .await
        .map_err(|error| failed(REVIEW_GET_TOOL_ID, error))?
        .map_err(|error| map_review_error(REVIEW_GET_TOOL_ID, error))?;
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
    store: Arc<ReviewStore>,
    source: Arc<dyn ReviewSourceProbe>,
}

impl ReviewFinalizeTool {
    #[must_use]
    pub fn new(store: Arc<ReviewStore>, source: Arc<dyn ReviewSourceProbe>) -> Self {
        Self { store, source }
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
        let review_id = required_text(REVIEW_FINALIZE_TOOL_ID, &params.review_id, "review_id")?;
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
        let store = Arc::clone(&self.store);
        let source = Arc::clone(&self.source);
        let session_id = ctx.session_id;
        let at_ms = now_ms(REVIEW_FINALIZE_TOOL_ID)?;
        let outcome = tokio::task::spawn_blocking(move || {
            let readiness = store.review(&session_id, &review_id)?.ok_or_else(|| {
                ReviewError::UnknownReview {
                    session_id: session_id.clone(),
                    review_id: review_id.clone(),
                }
            })?;
            let claims = store.claims(&session_id, &review_id)?;
            let refreshed_source = source
                .capture(&readiness.source.scope_paths, at_ms)
                .map_err(|error| ReviewError::CorruptEvent {
                    event_type: "review.source.probe".to_owned(),
                    detail: error.to_string(),
                })?;
            let source_changed = readiness.source.head_sha != refreshed_source.head_sha
                || readiness.source.worktree_digest != refreshed_source.worktree_digest
                || readiness.source.worktree_path != refreshed_source.worktree_path;
            let observed = claims
                .iter()
                .map(|claim| ObservedClaimEvidence {
                    claim_id: claim.id.clone(),
                    evidence: if source_changed {
                        Err(format!(
                            "review source changed from {} to {}",
                            readiness.source.worktree_digest, refreshed_source.worktree_digest
                        ))
                    } else {
                        claim
                            .evidence
                            .iter()
                            .map(|anchor| source.anchor(anchor))
                            .collect::<Result<Vec<_>, _>>()
                            .map_err(|error| error.to_string())
                    },
                })
                .collect::<Vec<_>>();
            store.finalize(FinalizeReview {
                session_id: &session_id,
                review_id: &review_id,
                expected_revision: params.expected_revision,
                requested_status: params.requested_status,
                load_bearing_claim_ids: &params.load_bearing_claim_ids,
                extra_blockers: &blockers,
                observed: &observed,
                at_ms,
            })
        })
        .await
        .map_err(|error| failed(REVIEW_FINALIZE_TOOL_ID, error))?
        .map_err(|error| map_review_error(REVIEW_FINALIZE_TOOL_ID, error))?;
        Ok(ToolOutput::text(
            format!("review {}", outcome.readiness.status),
            finalize_json(&outcome),
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
        "artifactPath": readiness.artifact_path,
        "planID": readiness.plan_id,
        "planRevision": readiness.plan_revision,
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
        "receiptID": readiness.receipt_id,
    })
}

fn finalize_json(outcome: &ReviewFinalizeOutcome) -> String {
    let mut blockers = outcome.blockers.clone();
    loop {
        let value = json!({
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
            "receiptID": outcome.readiness.receipt_id,
        },
        "blockers": outcome.blockers,
        "omittedBlockers": outcome.blockers.len().saturating_sub(blockers.len()),
        });
        let mut object = value;
        object["blockers"] = serde_json::to_value(&blockers).expect("blockers serialize");
        let encoded = object.to_string();
        if encoded.len() <= MAX_GET_BYTES {
            return encoded;
        }
        if blockers.pop().is_none() {
            return json!({
                "reviewID": outcome.readiness.review_id,
                "revision": outcome.readiness.revision,
                "status": outcome.readiness.status,
                "blockersOmitted": outcome.blockers.len(),
            })
            .to_string();
        }
    }
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

fn required_text(tool: &str, value: &str, field: &str) -> Result<String, ToolError> {
    let value = value.trim();
    if value.is_empty() {
        Err(invalid(tool, &format!("{field} must contain visible text")))
    } else {
        Ok(value.to_owned())
    }
}

fn required_option(tool: &str, value: &Option<String>, field: &str) -> Result<String, ToolError> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| invalid(tool, &format!("{field} is required for this action")))
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
        source: Box::new(std::io::Error::other(message.to_owned())),
    }
}

fn failed(tool: &str, error: impl std::fmt::Display) -> ToolError {
    ToolError::Failed {
        tool: tool.to_owned(),
        source: Box::new(std::io::Error::other(error.to_string())),
    }
}

fn now_ms(tool: &str) -> Result<i64, ToolError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| failed(tool, error))?;
    i64::try_from(elapsed.as_millis()).map_err(|error| failed(tool, error))
}

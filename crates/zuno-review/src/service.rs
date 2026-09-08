use crate::store::{ObservedClaimEvidence, ReconcileObservation};
use crate::{
    MAX_EVIDENCE_OBSERVATION_BYTES, ReviewClaim, ReviewError, ReviewReadiness, ReviewSourceProbe,
    ReviewSourceSnapshot, ReviewStore, SourceProbeError,
};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use zuno_runtime::{Component, PrepareContext, ProfileBundle, RuntimeError};
use zuno_tool::ToolContext;

const REVIEW_BUNDLE_ID: &str = "zuno.review";
const REVIEW_COMPONENT_ID: &str = "zuno.review";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewPlanBinding {
    pub id: String,
    pub revision: i64,
}

#[derive(Debug, Clone)]
pub struct ReviewOpenRequest<'a> {
    pub session_id: &'a str,
    pub artifact_path: Option<&'a str>,
    pub scope_paths: &'a [String],
    pub plan: Option<ReviewPlanBinding>,
    pub at_ms: i64,
}

pub trait ReviewPlanProbe: Send + Sync + 'static {
    fn current(&self, session_id: &str) -> Result<Option<ReviewPlanBinding>, String>;
}

#[async_trait]
pub trait ReviewCouncilRunner: Send + Sync + 'static {
    async fn run(
        &self,
        review: &ReviewReadiness,
        question: String,
        context: ToolContext,
    ) -> Result<Value, String>;
}

#[derive(Debug, Default)]
pub struct NoopReviewCouncilRunner;

#[async_trait]
impl ReviewCouncilRunner for NoopReviewCouncilRunner {
    async fn run(
        &self,
        _review: &ReviewReadiness,
        _question: String,
        _context: ToolContext,
    ) -> Result<Value, String> {
        Ok(Value::Null)
    }
}

#[derive(Debug, Default)]
pub struct NoReviewPlanProbe;

impl ReviewPlanProbe for NoReviewPlanProbe {
    fn current(&self, _session_id: &str) -> Result<Option<ReviewPlanBinding>, String> {
        Ok(None)
    }
}

#[derive(Clone)]
pub struct ReviewService {
    store: Arc<ReviewStore>,
    source: Arc<dyn ReviewSourceProbe>,
}

impl ReviewService {
    #[must_use]
    pub fn new(store: Arc<ReviewStore>, source: Arc<dyn ReviewSourceProbe>) -> Self {
        Self { store, source }
    }

    #[must_use]
    pub fn store(&self) -> Arc<ReviewStore> {
        Arc::clone(&self.store)
    }

    #[must_use]
    pub fn source(&self) -> Arc<dyn ReviewSourceProbe> {
        Arc::clone(&self.source)
    }

    pub fn capture(
        &self,
        scope_paths: &[String],
        artifact_path: Option<&str>,
        at_ms: i64,
    ) -> Result<ReviewSourceSnapshot, SourceProbeError> {
        self.source.capture(scope_paths, artifact_path, at_ms)
    }

    pub(crate) fn observe_claims(&self, claims: &[ReviewClaim]) -> Vec<ObservedClaimEvidence> {
        let requested = claims
            .iter()
            .flat_map(|claim| claim.evidence.iter().cloned())
            .collect::<Vec<_>>();
        let anchored = match self
            .source
            .anchor_batch(&requested, MAX_EVIDENCE_OBSERVATION_BYTES)
        {
            Ok(anchored) => anchored,
            Err(error) => {
                let detail = error.to_string();
                return claims
                    .iter()
                    .map(|claim| ObservedClaimEvidence {
                        claim_id: claim.id.clone(),
                        evidence: Err(detail.clone()),
                    })
                    .collect();
            }
        };
        let mut offset = 0_usize;
        claims
            .iter()
            .map(|claim| {
                let end = offset.saturating_add(claim.evidence.len());
                let evidence = anchored
                    .get(offset..end)
                    .map(<[crate::EvidenceAnchor]>::to_vec)
                    .ok_or_else(|| "review evidence batch returned the wrong size".to_owned());
                offset = end;
                ObservedClaimEvidence {
                    claim_id: claim.id.clone(),
                    evidence,
                }
            })
            .collect()
    }

    pub fn open_review(
        &self,
        request: ReviewOpenRequest<'_>,
        plans: &dyn ReviewPlanProbe,
    ) -> Result<ReviewReadiness, ReviewError> {
        let requested = request.plan;
        let plan_id = requested.as_ref().map(|binding| binding.id.as_str());
        let plan_revision = requested.as_ref().map(|binding| binding.revision);
        self.store.open_review(
            request.session_id,
            plan_id,
            plan_revision,
            true,
            || {
                let current = plans.current(request.session_id).map_err(|detail| {
                    ReviewError::CorruptEvent {
                        event_type: "review.plan.probe".to_owned(),
                        detail,
                    }
                })?;
                if let Some(requested) = requested.as_ref()
                    && current.as_ref() != Some(requested)
                {
                    return Err(ReviewError::PlanMismatch {
                        id: requested.id.clone(),
                        revision: requested.revision,
                    });
                }
                self.capture(request.scope_paths, request.artifact_path, request.at_ms)
                    .map_err(|error| ReviewError::CorruptEvent {
                        event_type: "review.source.probe".to_owned(),
                        detail: error.to_string(),
                    })
            },
            request.at_ms,
        )
    }

    pub fn reconcile(
        &self,
        session_id: &str,
        review_id: &str,
        plans: &dyn ReviewPlanProbe,
        at_ms: i64,
    ) -> Result<ReviewReadiness, ReviewError> {
        self.store
            .reconcile_external(session_id, review_id, at_ms, |current, claims| {
                let plan_current = match (current.plan_id.as_deref(), current.plan_revision) {
                    (Some(id), Some(revision)) => {
                        plans
                            .current(session_id)
                            .map_err(|detail| ReviewError::CorruptEvent {
                                event_type: "review.plan.probe".to_owned(),
                                detail,
                            })?
                            == Some(ReviewPlanBinding {
                                id: id.to_owned(),
                                revision,
                            })
                    }
                    _ => true,
                };
                let current_source = self
                    .capture(
                        &current.source.scope_paths,
                        current
                            .source
                            .artifact
                            .as_ref()
                            .map(|artifact| artifact.path.as_str()),
                        at_ms,
                    )
                    .map_err(|error| ReviewError::CorruptEvent {
                        event_type: "review.source.probe".to_owned(),
                        detail: error.to_string(),
                    })?;
                let observed = self.observe_claims(claims);
                Ok(ReconcileObservation {
                    current_source,
                    observed,
                    plan_current,
                })
            })
    }
}

struct ReviewComponent {
    service: Arc<ReviewService>,
}

#[async_trait]
impl Component for ReviewComponent {
    fn id(&self) -> &str {
        REVIEW_COMPONENT_ID
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        context.provide(Arc::clone(&self.service))
    }
}

#[must_use]
pub fn review_bundle(service: Arc<ReviewService>) -> ProfileBundle {
    ProfileBundle::new(REVIEW_BUNDLE_ID).with_component(ReviewComponent { service })
}

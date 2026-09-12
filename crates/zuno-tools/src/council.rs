//! Bounded instantiation of configuration-owned multi-model Councils.
//!
//! The caller selects one frozen preset and supplies a question. The preset owns
//! seats, agents, quorum, concurrency, deadlines, retries, and output bounds. Seat
//! model routing reuses [`crate::task::TaskTool`], so Council cannot introduce a
//! second provider or reasoning policy.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use zuno_error::ToolError;
use zuno_orchestration::{AttemptSnapshot, CouncilPresetDescriptor, sha256_json};
use zuno_tool::{
    PermissionAsk, ToolConcurrencyPolicy, ToolContext, ToolOutput, ToolReplayPolicy, ToolUiIntent,
    TypedTool,
};

use crate::task::{ChildTurnRequest, DelegationModelRequest, ReportDelivery, TaskTool};
use zuno_review::{ReportLimits, seat_response_contract};

/// Stable model-facing id for Council execution.
pub const WIRE_ID: &str = "council_run";
/// Council execution is independently permissioned from arbitrary child delegation.
pub const PERMISSION_KEY: &str = WIRE_ID;

const MAX_SEATS: usize = 12;
const MAX_RETRIES: usize = 3;
const MAX_DEADLINE_MS: u64 = 10 * 60 * 1_000;
const MAX_SEAT_OUTPUT_BYTES: usize = 64 * 1_024;
const MAX_SYNTHESIS_INPUT_BYTES: usize = 256 * 1_024;
const MAX_QUESTION_BYTES: usize = 64 * 1_024;
const MAX_TOOL_OUTPUT_BYTES: usize = 16 * 1_024;
const MAX_DESCRIPTION_CHARS: usize = 500;
const MAX_PRESET_CHARS: usize = 128;
const SEAT_RESPONSE_CONTRACT: &str = "Return exactly one JSON object and no markdown. Required fields: `verdict` (non-empty string), `confidence` (number from 0 to 1), `evidence` (array of strings), `risks` (array of strings), and `recommendation` (non-empty string). Do not include hidden reasoning or tool transcripts.";

/// Arguments for one immutable Council preset invocation.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CouncilParams {
    /// Configured Council preset name.
    pub preset: String,
    /// Decision or question every isolated seat evaluates.
    pub question: String,
    /// Durable review that owns every accepted seat claim.
    #[serde(default, rename = "reviewID")]
    pub review_id: Option<String>,
    /// Source snapshot every seat must echo back.
    #[serde(default, rename = "sourceSnapshotID")]
    pub source_snapshot_id: Option<String>,
    /// Short label shown in durable jobs and clients.
    #[serde(default)]
    pub description: Option<String>,
    /// Run asynchronously and return a durable job id immediately.
    #[serde(default)]
    pub background: Option<bool>,
    /// How a background terminal report reaches the parent session.
    #[serde(default, rename = "reportDelivery")]
    pub report_delivery: Option<ReportDelivery>,
}

/// One fully resolved Council seat admitted to the runtime.
#[derive(Debug, Clone, PartialEq)]
pub struct CouncilSeatRequest {
    pub id: String,
    pub turn: ChildTurnRequest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CouncilReviewBinding {
    pub review_id: String,
    pub source_snapshot_id: String,
}

/// A validated Council run admitted by the tool layer.
#[derive(Debug, Clone, PartialEq)]
pub struct CouncilRequest {
    pub parent_session_id: String,
    pub parent_attempt: Option<Arc<AttemptSnapshot>>,
    pub preset: String,
    pub description: Option<String>,
    pub question: String,
    pub preset_source_id: String,
    pub review: Option<CouncilReviewBinding>,
    pub seats: Vec<CouncilSeatRequest>,
    pub quorum: usize,
    pub max_parallel: usize,
    /// End-to-end wall-clock bound for seats plus synthesis.
    pub deadline: Duration,
    /// Maximum synthesis time reserved inside [`Self::deadline`].
    pub synthesis_timeout: Duration,
    pub max_retries: usize,
    pub seat_output_bytes: usize,
    pub synthesis_input_bytes: usize,
    pub background: bool,
    pub report_delivery: ReportDelivery,
}

/// Result returned by the Council runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CouncilTurn {
    pub run_id: String,
    pub job_id: Option<String>,
    pub output: String,
}

/// Council scheduling and synthesis effects supplied by the composition root.
#[async_trait]
pub trait CouncilHost: Send + Sync + 'static {
    async fn dispatch(
        &self,
        request: CouncilRequest,
        cancellation: CancellationToken,
    ) -> Result<CouncilTurn, String>;
}

/// Model-facing selection of a verified Council preset.
pub struct CouncilTool {
    presets: Vec<CouncilPresetDescriptor>,
    task: TaskTool,
    host: Arc<dyn CouncilHost>,
    description: String,
    allow_review_binding: bool,
}

impl CouncilTool {
    /// Bind frozen presets to the shared delegation planner and Council runtime.
    pub fn new(
        presets: impl IntoIterator<Item = CouncilPresetDescriptor>,
        task: TaskTool,
        host: Arc<dyn CouncilHost>,
    ) -> Result<Self, String> {
        let presets = presets.into_iter().collect::<Vec<_>>();
        if presets.is_empty() {
            return Err("council tool requires at least one configured preset".to_owned());
        }
        let targets = task.targets();
        let mut names = BTreeSet::new();
        for preset in &presets {
            validate_preset(preset, &targets)?;
            if !names.insert(preset.name.clone()) {
                return Err(format!(
                    "council preset `{}` is registered more than once",
                    preset.name
                ));
            }
        }
        let available = presets
            .iter()
            .map(|preset| preset.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Ok(Self {
            presets,
            task,
            host,
            description: format!(
                "Run one validated Council preset. The seats, agents, model routes, quorum, concurrency, retry policy, end-to-end deadline, reserved synthesis time, and output bounds are configuration-owned. Available presets: {available}."
            ),
            allow_review_binding: false,
        })
    }

    #[must_use]
    pub fn with_review_binding_authority(mut self, allowed: bool) -> Self {
        self.allow_review_binding = allowed;
        self
    }

    fn preset(&self, name: &str) -> Option<&CouncilPresetDescriptor> {
        self.presets.iter().find(|preset| preset.name == name)
    }

    fn expand(
        &self,
        preset: &CouncilPresetDescriptor,
        params: &CouncilParams,
        parent_session_id: &str,
        parent_attempt: Option<&Arc<AttemptSnapshot>>,
    ) -> Vec<CouncilSeatRequest> {
        let workflow = format!("council:{}", preset.name);
        let contract = if params.review_id.is_some() {
            seat_response_contract(ReportLimits::for_seat(preset.seat_output_bytes))
        } else {
            SEAT_RESPONSE_CONTRACT.to_owned()
        };
        preset
            .seats
            .iter()
            .map(|seat| {
                let review_context = params
                    .review_id
                    .as_deref()
                    .zip(params.source_snapshot_id.as_deref())
                    .map_or_else(String::new, |(review_id, source_snapshot_id)| {
                        format!(
                            "\nReview id: `{review_id}`\nSource snapshot id: `{source_snapshot_id}`\n"
                        )
                    });
                let prompt = format!(
                    "Council question:\n{}{}\nSeat `{}` instruction:\n{}\n\nBudget: {} ms total; {} ms shared by the seat phase (including queueing and retries); {} ms reserved for synthesis. The host supplies the actual remaining deadline at attempt admission. Return focused evidence and explicit unknowns within the budget; incomplete coverage is not a passing review.\n\n{}",
                    params.question, review_context, seat.id, seat.instruction,
                    preset.deadline_ms,
                    preset.deadline_ms.saturating_sub(preset.synthesis_policy.timeout_ms),
                    preset.synthesis_policy.timeout_ms,
                    contract
                );
                let description = Some(format!("{} / {}", preset.name, seat.id));
                let plan = self
                    .task
                    .plan(&seat.agent, None, &DelegationModelRequest::default());
                CouncilSeatRequest {
                    id: seat.id.clone(),
                    turn: ChildTurnRequest {
                        parent_session_id: parent_session_id.to_owned(),
                        parent_attempt: parent_attempt.map(Arc::clone),
                        workflow: Some(workflow.clone()),
                        workflow_node: Some(seat.id.clone()),
                        resume_session_id: None,
                        logical_key: format!(
                            "council:v1:{}",
                            sha256_json(&json!({
                                "preset": preset.name,
                                "seat": seat.id,
                                "agent": seat.agent,
                                "question": params.question,
                                "review_id": params.review_id,
                                "source_snapshot_id": params.source_snapshot_id,
                                "instruction": seat.instruction,
                            }))
                        ),
                        agent: seat.agent.clone(),
                        description,
                        prompt,
                        model: plan.model,
                        effort: plan.effort,
                        provider_options: plan.provider_options,
                        subagent_model_policy: self.task.subagent_model_policy(),
                        requested_model: None,
                        requested_effort: None,
                        background: false,
                        report_delivery: ReportDelivery::Quiet,
                    },
                }
            })
            .collect()
    }
}

#[async_trait]
impl TypedTool for CouncilTool {
    type Params = CouncilParams;

    fn id(&self) -> &str {
        WIRE_ID
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Never
    }

    fn concurrency_policy(&self) -> ToolConcurrencyPolicy {
        ToolConcurrencyPolicy::IsolatedBackground
    }

    fn ui_intent(&self) -> ToolUiIntent {
        ToolUiIntent::Subagent
    }

    async fn run(&self, params: CouncilParams, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let name = params.preset.trim();
        if name.is_empty() {
            return Err(invalid("`preset` must not be empty"));
        }
        if name.chars().count() > MAX_PRESET_CHARS {
            return Err(invalid(&format!(
                "`preset` exceeds the {MAX_PRESET_CHARS}-character limit"
            )));
        }
        let question = params.question.trim();
        if question.is_empty() {
            return Err(invalid("`question` must not be empty"));
        }
        if params.question.len() > MAX_QUESTION_BYTES {
            return Err(invalid(&format!(
                "`question` exceeds the {MAX_QUESTION_BYTES}-byte Council limit"
            )));
        }
        if params.description.as_deref().is_some_and(|description| {
            description.trim().is_empty() || description.chars().count() > MAX_DESCRIPTION_CHARS
        }) {
            return Err(invalid(&format!(
                "`description` must contain 1 to {MAX_DESCRIPTION_CHARS} characters"
            )));
        }
        let review = match (
            params.review_id.as_deref().map(str::trim),
            params.source_snapshot_id.as_deref().map(str::trim),
        ) {
            (None, None) => None,
            (Some(review_id), Some(source_snapshot_id))
                if !review_id.is_empty() && !source_snapshot_id.is_empty() =>
            {
                if ctx.agent != "review" {
                    return Err(invalid(
                        "`reviewID` and `sourceSnapshotID` are reserved for the native review Agent",
                    ));
                }
                if !self.allow_review_binding {
                    return Err(invalid(
                        "the active review Agent is not the unmodified native review profile",
                    ));
                }
                if name != "balanced-review" {
                    return Err(invalid(
                        "the native review Agent may bind only the `balanced-review` preset",
                    ));
                }
                Some(CouncilReviewBinding {
                    review_id: review_id.to_owned(),
                    source_snapshot_id: source_snapshot_id.to_owned(),
                })
            }
            _ => {
                return Err(invalid(
                    "`reviewID` and `sourceSnapshotID` must be supplied together and contain visible text",
                ));
            }
        };
        if self.allow_review_binding && review.is_none() {
            return Err(invalid(
                "the native review Agent must bind `balanced-review` to review_open output",
            ));
        }
        let background = params.background.unwrap_or(false);
        if !background && params.report_delivery.is_some() {
            return Err(invalid(
                "`reportDelivery` requires `background: true`; remove it or run in background",
            ));
        }
        let preset = self.preset(name).ok_or_else(|| {
            invalid(&format!(
                "unknown Council preset `{name}`; choose one of: {}",
                self.presets
                    .iter()
                    .map(|preset| preset.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        self.task.guard_depth(&ctx).await?;

        let mut metadata = Map::new();
        metadata.insert("preset".to_owned(), Value::String(name.to_owned()));
        metadata.insert(
            "seats".to_owned(),
            Value::Number(serde_json::Number::from(preset.seats.len() as u64)),
        );
        metadata.insert(
            "quorum".to_owned(),
            Value::Number(serde_json::Number::from(preset.quorum as u64)),
        );
        if let Some(description) = &params.description {
            metadata.insert("description".to_owned(), Value::String(description.clone()));
        }
        ctx.ask(
            WIRE_ID,
            PermissionAsk {
                permission: PERMISSION_KEY.to_owned(),
                patterns: vec![format!("council:{name}")],
                metadata,
                always: vec![format!("council:{name}")],
                ..PermissionAsk::default()
            },
        )
        .await?;

        let parent_session_id = ctx.session_id.clone();
        let parent_attempt = ctx.orchestration_snapshot().cloned();
        let request = CouncilRequest {
            parent_session_id: parent_session_id.clone(),
            parent_attempt: parent_attempt.clone(),
            preset: name.to_owned(),
            description: params.description.clone(),
            question: params.question.clone(),
            preset_source_id: preset.source_id.clone(),
            review,
            seats: self.expand(preset, &params, &parent_session_id, parent_attempt.as_ref()),
            quorum: preset.quorum,
            max_parallel: preset.max_parallel,
            deadline: Duration::from_millis(preset.deadline_ms),
            synthesis_timeout: Duration::from_millis(preset.synthesis_policy.timeout_ms),
            max_retries: preset.retry_policy.max_retries,
            seat_output_bytes: preset.seat_output_bytes,
            synthesis_input_bytes: preset.synthesis_policy.max_input_bytes,
            background,
            report_delivery: params.report_delivery.unwrap_or_default(),
        };
        let cancellation = CancellationToken::new();
        let dispatch = self.host.dispatch(request, cancellation.clone());
        tokio::pin!(dispatch);
        let turn = if background {
            dispatch.await
        } else {
            tokio::select! {
                result = &mut dispatch => result,
                () = ctx.interrupt.notified() => {
                    cancellation.cancel();
                    dispatch.await
                }
            }
        }
        .map_err(failed)?;
        if background && turn.job_id.is_none() {
            return Err(failed(
                "a background Council dispatch did not return a durable job id".to_owned(),
            ));
        }
        Ok(render(&params, &turn))
    }
}

fn validate_preset(preset: &CouncilPresetDescriptor, targets: &[String]) -> Result<(), String> {
    if preset.name.trim().is_empty()
        || preset.source_id.trim().is_empty()
        || preset.seats.is_empty()
        || preset.seats.len() > MAX_SEATS
        || preset.quorum == 0
        || preset.quorum > preset.seats.len()
        || preset.max_parallel == 0
        || preset.max_parallel > preset.seats.len()
        || preset.deadline_ms == 0
        || preset.deadline_ms > MAX_DEADLINE_MS
        || preset.synthesis_policy.timeout_ms == 0
        || preset.synthesis_policy.timeout_ms >= preset.deadline_ms
        || preset.retry_policy.max_retries > MAX_RETRIES
        || preset.seat_output_bytes == 0
        || preset.seat_output_bytes > MAX_SEAT_OUTPUT_BYTES
        || preset.synthesis_policy.max_input_bytes == 0
        || preset.synthesis_policy.max_input_bytes > MAX_SYNTHESIS_INPUT_BYTES
    {
        return Err(format!(
            "council descriptor `{}` has invalid identity, seats, quorum, or bounds",
            preset.name
        ));
    }
    let mut seat_ids = BTreeSet::new();
    for seat in &preset.seats {
        if seat.id.trim().is_empty()
            || seat.instruction.trim().is_empty()
            || !seat_ids.insert(seat.id.as_str())
        {
            return Err(format!(
                "council descriptor `{}` has an empty or duplicate seat",
                preset.name
            ));
        }
        if !targets.iter().any(|target| target == &seat.agent) {
            return Err(format!(
                "council.{}.seats.{} targets agent `{}` outside the current agent's delegate allowlist ({})",
                preset.name,
                seat.id,
                seat.agent,
                targets.join(", ")
            ));
        }
    }
    Ok(())
}

fn render(params: &CouncilParams, turn: &CouncilTurn) -> ToolOutput {
    let state = if turn.job_id.is_some() {
        "running"
    } else {
        "completed"
    };
    let delivery = match params.report_delivery.unwrap_or_default() {
        ReportDelivery::NextStep => "nextStep",
        ReportDelivery::Quiet => "quiet",
    };
    let body = bounded_council_output(json!({
        "kind": "council",
        "preset": params.preset,
        "runID": turn.run_id,
        "jobID": turn.job_id,
        "state": state,
        "reportDelivery": delivery,
        "description": params.description,
        "result": turn.output,
    }));
    ToolOutput::text(
        params
            .description
            .clone()
            .unwrap_or_else(|| format!("{} Council", params.preset)),
        body.clone(),
    )
    .with_metadata(
        "subagent",
        json!({
            "kind":"council",
            "preset":params.preset,
            "runID":turn.run_id,
            "jobID":turn.job_id,
            "state":state,
            "reportDelivery":delivery,
            "description":params.description,
            "result":body,
        }),
    )
}

fn bounded_council_output(value: Value) -> String {
    let encoded = value.to_string();
    if encoded.len() <= MAX_TOOL_OUTPUT_BYTES {
        return encoded;
    }
    json!({
        "kind": "council",
        "omitted": true,
        "reason": "Council result exceeded the tool output byte limit",
    })
    .to_string()
}

fn invalid(message: &str) -> ToolError {
    ToolError::InvalidArgs {
        tool: WIRE_ID.to_owned(),
        source: Box::new(std::io::Error::other(message.to_owned())),
    }
}

fn failed(message: String) -> ToolError {
    ToolError::Failed {
        tool: WIRE_ID.to_owned(),
        source: Box::new(std::io::Error::other(message)),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use zuno_orchestration::{
        CouncilRetryPolicyDescriptor, CouncilSeatDescriptor, CouncilSynthesisPolicyDescriptor,
    };
    use zuno_tool::{AllowAll, NeverInterrupted};

    #[derive(Default)]
    struct RecordingCouncilHost {
        requests: Mutex<Vec<CouncilRequest>>,
        background_job: bool,
    }

    #[async_trait]
    impl CouncilHost for RecordingCouncilHost {
        async fn dispatch(
            &self,
            request: CouncilRequest,
            _cancellation: CancellationToken,
        ) -> Result<CouncilTurn, String> {
            let background = request.background;
            self.requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(request);
            Ok(CouncilTurn {
                run_id: "council_test".to_owned(),
                job_id: (background && self.background_job).then(|| "job_test".to_owned()),
                output: "synthesis".to_owned(),
            })
        }
    }

    fn preset() -> CouncilPresetDescriptor {
        CouncilPresetDescriptor {
            name: "balanced-review".to_owned(),
            source_id: "test://council/balanced-review".to_owned(),
            quorum: 2,
            max_parallel: 2,
            deadline_ms: 5_000,
            seat_output_bytes: 4_096,
            retry_policy: CouncilRetryPolicyDescriptor { max_retries: 1 },
            synthesis_policy: CouncilSynthesisPolicyDescriptor {
                timeout_ms: 1_000,
                max_input_bytes: 8_192,
            },
            seats: vec![
                CouncilSeatDescriptor {
                    id: "evidence".to_owned(),
                    agent: "explorer".to_owned(),
                    instruction: "Collect evidence.".to_owned(),
                },
                CouncilSeatDescriptor {
                    id: "review".to_owned(),
                    agent: "oracle".to_owned(),
                    instruction: "Review risks.".to_owned(),
                },
            ],
        }
    }

    fn task() -> TaskTool {
        TaskTool::new(
            Arc::new(crate::task::RecordingHost::new()),
            Arc::new(crate::task::NoProviders),
        )
        .with_targets(
            crate::task::DelegationTargets::new(["explorer".to_owned(), "oracle".to_owned()])
                .expect("targets"),
        )
    }

    fn orchestration_snapshot() -> Arc<AttemptSnapshot> {
        Arc::new(
            serde_json::from_value(json!({
                "schemaVersion": 4,
                "turnId": "turn-parent",
                "step": 1,
                "capability": {
                    "schemaVersion": 4,
                    "pack": {"id":"test","version":"1","upstreamRevision":"test"},
                    "extensionRevision": 0,
                    "permissionPolicySha256": "policy",
                    "sandbox": {
                        "mode": "workspace-write",
                        "network": "deny",
                        "writableRoots": [],
                        "protectedPaths": []
                    },
                    "profiles": [], "presets": [], "councils": [], "workflows": [], "skills": []
                },
                "owner": {
                    "sessionId":"ses_parent", "parentSessionId":null, "parentAttempt":null,
                    "workflow":null, "workflowNode":null
                },
                "agent": {
                    "name":"build", "sourceId":"test://build",
                    "definitionSha256":"definition", "permissionSha256":"permission",
                    "promptPolicySha256":"prompt"
                },
                "model": {
                    "providerId":"fake", "modelId":"fake-model", "wireModelId":"fake-model",
                    "surface":"responses", "reasoningSha256":"reasoning", "preset":null
                },
                "selectedSkills": [],
                "prompt": {"eventId":"evt-parent","assemblySha256":"assembly","actualSha256":"actual"},
                "tools": []
            }))
            .expect("test orchestration snapshot"),
        )
    }

    fn params(background: bool) -> CouncilParams {
        CouncilParams {
            preset: "balanced-review".to_owned(),
            question: "Should this change ship?".to_owned(),
            review_id: None,
            source_snapshot_id: None,
            description: Some("release review".to_owned()),
            background: background.then_some(true),
            report_delivery: None,
        }
    }

    fn review_params() -> CouncilParams {
        CouncilParams {
            review_id: Some("rev_1".to_owned()),
            source_snapshot_id: Some("rsnap_1".to_owned()),
            ..params(false)
        }
    }

    #[tokio::test]
    async fn expands_only_frozen_seats_and_preserves_parent_attempt() {
        let host = Arc::new(RecordingCouncilHost::default());
        let snapshot = orchestration_snapshot();
        let tool = CouncilTool::new([preset()], task(), host.clone()).expect("Council tool");
        let output = tool
            .run(
                params(false),
                ToolContext::new(
                    "ses_parent",
                    "msg_parent",
                    "call_council",
                    "build",
                    Arc::new(AllowAll),
                    Arc::new(NeverInterrupted),
                )
                .with_orchestration_snapshot(Arc::clone(&snapshot)),
            )
            .await
            .expect("Council runs");
        assert!(output.output.contains("\"kind\":\"council\""));
        let requests = host
            .requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].quorum, 2);
        assert_eq!(requests[0].max_parallel, 2);
        assert_eq!(requests[0].max_retries, 1);
        assert_eq!(requests[0].synthesis_timeout, Duration::from_secs(1));
        assert_eq!(requests[0].seats.len(), 2);
        assert!(Arc::ptr_eq(
            requests[0].parent_attempt.as_ref().expect("parent attempt"),
            &snapshot
        ));
        for seat in &requests[0].seats {
            assert!(Arc::ptr_eq(
                seat.turn.parent_attempt.as_ref().expect("seat attempt"),
                &snapshot
            ));
            assert_eq!(
                seat.turn.workflow.as_deref(),
                Some("council:balanced-review")
            );
            assert_eq!(seat.turn.workflow_node.as_deref(), Some(seat.id.as_str()));
            assert!(seat.turn.prompt.contains(SEAT_RESPONSE_CONTRACT));
            assert!(seat.turn.prompt.contains("4000 ms shared"));
            assert!(!seat.turn.prompt.contains("Source snapshot id"));
            assert!(!seat.turn.background);
        }
    }

    #[tokio::test]
    async fn review_binding_requires_native_authority_and_the_balanced_preset() {
        let host = Arc::new(RecordingCouncilHost::default());
        let untrusted = CouncilTool::new([preset()], task(), host.clone()).expect("Council tool");
        let review_context = || {
            ToolContext::new(
                "ses_parent",
                "msg_parent",
                "call_council",
                "review",
                Arc::new(AllowAll),
                Arc::new(NeverInterrupted),
            )
        };
        assert!(
            untrusted
                .run(review_params(), review_context())
                .await
                .is_err(),
            "a same-named overridden review Agent must not gain review-binding authority"
        );
        untrusted
            .run(params(false), review_context())
            .await
            .expect("a same-named overridden review Agent retains generic Council access");

        let trusted = CouncilTool::new([preset()], task(), host.clone())
            .expect("Council tool")
            .with_review_binding_authority(true);
        assert!(
            trusted
                .run(
                    review_params(),
                    ToolContext::new(
                        "ses_parent",
                        "msg_parent",
                        "call_council",
                        "build",
                        Arc::new(AllowAll),
                        Arc::new(NeverInterrupted),
                    ),
                )
                .await
                .is_err(),
            "another Agent cannot attach reports to a review"
        );
        trusted
            .run(review_params(), review_context())
            .await
            .expect("native review Council");
        let requests = host
            .requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let request = requests.last().expect("review request");
        assert!(request.review.is_some());
        for seat in &request.seats {
            assert!(
                seat.turn
                    .prompt
                    .contains(&seat_response_contract(ReportLimits::for_seat(
                        request.seat_output_bytes
                    )))
            );
        }
    }

    #[tokio::test]
    async fn unknown_preset_and_empty_question_fail_before_dispatch() {
        let host = Arc::new(RecordingCouncilHost::default());
        let tool = CouncilTool::new([preset()], task(), host.clone()).expect("Council tool");
        let context = || {
            ToolContext::new(
                "ses_parent",
                "msg_parent",
                "call_council",
                "build",
                Arc::new(AllowAll),
                Arc::new(NeverInterrupted),
            )
        };
        let mut unknown = params(false);
        unknown.preset = "invented".to_owned();
        assert!(tool.run(unknown, context()).await.is_err());
        let mut empty = params(false);
        empty.question = "  ".to_owned();
        assert!(tool.run(empty, context()).await.is_err());
        assert!(
            host.requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty()
        );
    }

    #[test]
    fn synthesis_timeout_must_fit_inside_the_total_deadline() {
        let mut invalid = preset();
        invalid.synthesis_policy.timeout_ms = invalid.deadline_ms;
        let error =
            match CouncilTool::new([invalid], task(), Arc::new(RecordingCouncilHost::default())) {
                Ok(_) => panic!("an empty seat-phase budget must be rejected"),
                Err(error) => error,
            };
        assert!(error.contains("invalid identity, seats, quorum, or bounds"));
    }

    #[tokio::test]
    async fn background_dispatch_must_return_a_durable_job_id() {
        let host = Arc::new(RecordingCouncilHost::default());
        let tool = CouncilTool::new([preset()], task(), host).expect("Council tool");
        let error = tool
            .run(
                params(true),
                ToolContext::new(
                    "ses_parent",
                    "msg_parent",
                    "call_council",
                    "build",
                    Arc::new(AllowAll),
                    Arc::new(NeverInterrupted),
                ),
            )
            .await
            .expect_err("missing job id must fail");
        match error {
            ToolError::Failed { source, .. } => {
                assert!(source.to_string().contains("durable job id"));
            }
            other => panic!("expected failed Council dispatch, got {other:?}"),
        }
    }

    #[test]
    fn policies_mark_council_as_non_replayable_isolated_subagent_work() {
        let tool = CouncilTool::new(
            [preset()],
            task(),
            Arc::new(RecordingCouncilHost::default()),
        )
        .expect("Council tool");
        assert_eq!(tool.id(), WIRE_ID);
        assert_eq!(tool.replay_policy(), ToolReplayPolicy::Never);
        assert_eq!(
            tool.concurrency_policy(),
            ToolConcurrencyPolicy::IsolatedBackground
        );
        assert_eq!(tool.ui_intent(), ToolUiIntent::Subagent);
    }
}

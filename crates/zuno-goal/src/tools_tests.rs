use super::*;
use crate::GoalStatus;
use serde_json::json;
use std::sync::Arc;
use zuno_tool::{AllowAll, NeverInterrupted, ToolReplayPolicy};

struct Fixture {
    store: Arc<GoalStore>,
    _spill: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let spill = tempfile::tempdir().expect("create spill directory");
        let store = GoalStore::open_memory(spill.path().to_owned()).expect("open goal store");
        Self {
            store: Arc::new(store),
            _spill: spill,
        }
    }

    fn context(&self, call_id: &str) -> ToolContext {
        ToolContext::new(
            "ses_tools",
            "msg_tools",
            call_id,
            "orchestrator",
            Arc::new(AllowAll),
            Arc::new(NeverInterrupted),
        )
    }
}

#[test]
fn goal_descriptions_encode_creation_authority_and_terminal_audits() {
    assert!(
        CREATE_DESCRIPTION.contains("only when explicitly requested"),
        "{CREATE_DESCRIPTION}"
    );
    assert!(
        CREATE_DESCRIPTION.contains("Do not infer a goal from ordinary work"),
        "{CREATE_DESCRIPTION}"
    );
    assert!(
        UPDATE_DESCRIPTION.contains("Read the current goal before changing it"),
        "{UPDATE_DESCRIPTION}"
    );
    assert!(
        UPDATE_DESCRIPTION.contains("Do not use this tool merely because a turn is ending"),
        "{UPDATE_DESCRIPTION}"
    );
}

#[tokio::test]
async fn all_three_tools_share_one_authoritative_goal() {
    let fixture = Fixture::new();
    let tools = goal_tools(Arc::clone(&fixture.store));
    assert_eq!(
        tools.iter().map(|tool| tool.id()).collect::<Vec<_>>(),
        [GET_GOAL_TOOL_ID, CREATE_GOAL_TOOL_ID, UPDATE_GOAL_TOOL_ID]
    );
    assert_eq!(tools[0].replay_policy(), ToolReplayPolicy::Safe);
    assert_eq!(tools[1].replay_policy(), ToolReplayPolicy::Never);
    assert_eq!(tools[2].replay_policy(), ToolReplayPolicy::Never);

    let created = tools[1]
        .execute(
            json!({
                "objective": "ship task 68",
                "success_criteria": ["release artifact exists", "workspace gates pass"],
                "token_budget": 9000
            }),
            fixture.context("call_create"),
        )
        .await
        .expect("create goal");
    let created_goal = goal_from_metadata(&created)
        .expect("decode create metadata")
        .expect("created goal");
    assert_eq!(created_goal.objective, "ship task 68");
    assert_eq!(
        created_goal.success_criteria,
        ["release artifact exists", "workspace gates pass"]
    );
    assert_eq!(created_goal.token_budget, Some(9000));
    assert_eq!(created_goal.status, GoalStatus::Active);

    let read = tools[0]
        .execute(json!({}), fixture.context("call_get"))
        .await
        .expect("get goal");
    assert_eq!(
        goal_from_metadata(&read).expect("decode get metadata"),
        Some(created_goal)
    );

    // The criteria this goal recorded gate its completion, so the same call closes
    // them: the point here is that the three tools agree on one goal, not that a
    // checklist can be skipped.
    let completed = tools[2]
        .execute(
            json!({
                "expected_revision": 1,
                "status": "complete",
                "waive_criteria": [
                    {"criterionId": "c1", "reason": "the artifact is built by release tooling"},
                    {"criterionId": "c2", "reason": "the gates run in CI for this fixture"}
                ]
            }),
            fixture.context("call_update"),
        )
        .await
        .expect("complete goal");
    assert_eq!(
        goal_from_metadata(&completed)
            .expect("decode update metadata")
            .expect("updated goal")
            .status,
        GoalStatus::Complete
    );
}

#[tokio::test]
async fn update_schema_and_deserializer_reject_system_owned_statuses() {
    let fixture = Fixture::new();
    fixture
        .store
        .create_goal("ses_tools", "keep ownership split", None)
        .expect("create goal");
    let tool = erase(UpdateGoalTool::new(Arc::clone(&fixture.store)));
    let definition = tool.definition();
    let rendered_schema = definition.parameters.to_string();
    assert!(rendered_schema.contains("complete"), "{rendered_schema}");
    assert!(rendered_schema.contains("blocked"), "{rendered_schema}");
    assert!(!rendered_schema.contains("paused"), "{rendered_schema}");

    let error = tool
        .execute(
            json!({"expected_revision": 1, "status": "paused"}),
            fixture.context("call_paused"),
        )
        .await
        .expect_err("paused is system-owned");
    assert!(matches!(error, ToolError::InvalidArgs { .. }));
    assert_eq!(
        fixture
            .store
            .goal("ses_tools")
            .expect("read goal")
            .expect("goal exists")
            .status,
        GoalStatus::Active
    );
}

#[tokio::test]
async fn model_refusals_are_correctable_and_internal_failures_are_not_forged() {
    let fixture = Fixture::new();
    let create = erase(CreateGoalTool::new(Arc::clone(&fixture.store)));
    create
        .execute(
            json!({"objective": "first", "success_criteria": ["the first check passes"]}),
            fixture.context("call_first"),
        )
        .await
        .expect("first goal");

    let conflict = create
        .execute(
            json!({"objective": "second", "success_criteria": ["the second check passes"]}),
            fixture.context("call_second"),
        )
        .await
        .expect_err("unfinished goal cannot be replaced");
    assert!(matches!(conflict, ToolError::InvalidArgs { .. }));
    assert!(conflict.is_model_correctable());

    let invalid_budget = create
        .execute(
            json!({
                "objective": "second",
                "success_criteria": ["the second check passes"],
                "token_budget": 0
            }),
            fixture.context("call_budget"),
        )
        .await
        .expect_err("zero budget is invalid");
    assert!(matches!(invalid_budget, ToolError::InvalidArgs { .. }));
}

#[tokio::test]
async fn get_without_a_goal_returns_structured_null() {
    let fixture = Fixture::new();
    let output = erase(GetGoalTool::new(Arc::clone(&fixture.store)))
        .execute(json!({}), fixture.context("call_empty"))
        .await
        .expect("get empty goal");
    assert_eq!(output.title, "No goal");
    assert_eq!(goal_from_metadata(&output).expect("decode metadata"), None);
}

#[tokio::test]
async fn update_rejects_a_stale_goal_revision_without_mutating_state() {
    let fixture = Fixture::new();
    let created = fixture
        .store
        .create_goal("ses_tools", "preserve concurrent work", None)
        .expect("create goal");
    fixture
        .store
        .update_objective("ses_tools", "newer objective")
        .expect("advance revision")
        .expect("goal exists");
    let tool = erase(UpdateGoalTool::new(Arc::clone(&fixture.store)));
    let error = tool
        .execute(
            json!({
                "expected_revision": created.revision,
                "status": "complete"
            }),
            fixture.context("call_stale"),
        )
        .await
        .expect_err("stale revision must be rejected");
    assert!(matches!(error, ToolError::InvalidArgs { .. }));
    let current = fixture
        .store
        .goal("ses_tools")
        .expect("read goal")
        .expect("goal exists");
    assert_eq!(current.objective, "newer objective");
    assert_eq!(current.status, GoalStatus::Active);
}

#[tokio::test]
async fn blocked_update_requires_three_persisted_matching_failure_signals() {
    let fixture = Fixture::new();
    fixture
        .store
        .create_goal("ses_tools", "enforce blocked audit", None)
        .expect("create goal");
    let tool = erase(UpdateGoalTool::new(Arc::clone(&fixture.store)));

    let continuation = crate::GoalContinuation::new(
        Arc::clone(&fixture.store),
        zuno_engine::status::SessionRunRegistry::new(),
    );
    for turn in 1..=3 {
        let pending = tool
            .execute(
                json!({
                    "expected_revision": 1,
                    "status": "blocked",
                    "blocking_condition": "credential unavailable"
                }),
                fixture.context(&format!("call_blocked_{turn}")),
            )
            .await
            .expect("stage blocker for this turn");
        assert_eq!(
            goal_from_metadata(&pending)
                .expect("decode pending metadata")
                .expect("goal exists")
                .status,
            GoalStatus::Active
        );
        continuation
            .record_turn_outcome("ses_tools", crate::GoalTurnOutcome::Progress)
            .expect("settle one real turn");
    }
    let blocked = fixture
        .store
        .goal("ses_tools")
        .expect("read blocked goal")
        .expect("goal exists");
    assert_eq!(blocked.status, GoalStatus::Blocked);
    assert_eq!(
        blocked.blocked_reason.as_deref(),
        Some("credential unavailable")
    );
}

#[tokio::test]
async fn repeated_blocked_calls_in_one_turn_count_once() {
    let fixture = Fixture::new();
    fixture
        .store
        .create_goal("ses_tools", "do not forge turns", None)
        .expect("create goal");
    let tool = erase(UpdateGoalTool::new(Arc::clone(&fixture.store)));
    for call in 1..=3 {
        tool.execute(
            json!({"expected_revision": 1, "status": "blocked", "blocking_condition": "same blocker"}),
            fixture.context(&format!("call_retry_{call}")),
        )
        .await
        .expect("restage blocker within one turn");
    }
    let continuation = crate::GoalContinuation::new(
        Arc::clone(&fixture.store),
        zuno_engine::status::SessionRunRegistry::new(),
    );
    let audit = continuation
        .record_turn_outcome("ses_tools", crate::GoalTurnOutcome::Progress)
        .expect("settle one real turn");
    assert!(matches!(
        audit,
        crate::BlockedAudit::Pending(crate::FailureStreak {
            consecutive_turns: 1,
            ..
        })
    ));
}

#[test]
fn a_terminal_status_discards_an_unsettled_blocker() {
    let fixture = Fixture::new();
    fixture
        .store
        .create_goal("ses_tools", "discard stale blocker", None)
        .expect("create goal");
    assert!(
        fixture
            .store
            .stage_failure_signal("ses_tools", "old blocker")
            .expect("stage blocker")
    );
    fixture
        .store
        .update_status_as_model("ses_tools", crate::ModelStatus::Blocked)
        .expect("block goal");
    assert_eq!(
        fixture
            .store
            .consume_staged_failure_signal("ses_tools")
            .expect("read pending blocker"),
        None
    );
}

/// The message the reporter shows the model, which lives on the chained cause.
fn refusal_detail(error: &ToolError) -> String {
    std::error::Error::source(error).map_or_else(|| error.to_string(), ToString::to_string)
}

/// Store an authoritative passing receipt for the tool session to cite.
///
/// A goal in the session is also moved behind `time_created`. These tests cite
/// receipts at small synthetic times while a goal stamps its creation from the wall
/// clock, and the evidence gate refuses a check that ran before its goal existed,
/// so without this the fixture would be citing a receipt from before the goal.
fn record_passing_receipt(fixture: &Fixture, id: &str, time_created: i64) {
    let connection = fixture.store.pool().get().expect("check out connection");
    connection
        .execute(
            "UPDATE goal SET created_at_ms = MIN(created_at_ms, ?2) WHERE session_id = ?1",
            rusqlite::params!["ses_tools", time_created],
        )
        .expect("move the goal behind the receipt");
    zuno_db::verification::record(
        &connection,
        &zuno_db::verification::NewVerificationReceipt {
            id: id.to_owned(),
            session_id: "ses_tools".to_owned(),
            turn_id: Some("turn_tools".to_owned()),
            tool_call_id: format!("call_verify_{id}"),
            tool_id: "shell".to_owned(),
            summary: "cargo test".to_owned(),
            workdir: None,
            exit_code: Some(0),
            exit_authority: zuno_db::verification::ExitAuthority::Authoritative,
            outcome: zuno_db::verification::ReceiptOutcome::Passed,
            git_head: None,
            output_digest: None,
            detail: None,
            time_created,
        },
    )
    .expect("record verification receipt");
}

#[test]
fn goal_descriptions_teach_the_evidence_contract() {
    assert!(
        CREATE_DESCRIPTION.contains("success_criteria is required"),
        "the model must be told the checklist is not optional, because omitting it is what \
         used to skip the whole audit: {CREATE_DESCRIPTION}"
    );
    assert!(
        !CREATE_DESCRIPTION.contains("record them only for checks you intend to close"),
        "nothing may present a recorded criterion as a liability, which reads as advice to \
         record none: {CREATE_DESCRIPTION}"
    );
    assert!(
        !UPDATE_DESCRIPTION.contains("editing files after a criterion was satisfied reopens it"),
        "an unconditional promise that edits reopen criteria is false for an edit no tool \
         reported, and teaches the model to trust a criterion that stayed closed: \
         {UPDATE_DESCRIPTION}"
    );
    assert!(
        UPDATE_DESCRIPTION.contains("receipt id"),
        "{UPDATE_DESCRIPTION}"
    );
    assert!(
        UPDATE_DESCRIPTION.contains("reopen"),
        "{UPDATE_DESCRIPTION}"
    );
}

/// The reported bypass, run as the reporter ran it: propose a goal with no
/// `success_criteria`, edit the workspace through `shell` — which reports no written
/// path, so nothing escalates the goal and nothing stamps a mutation mark — then
/// report `complete`. Every step after the first is unreachable now, because no goal
/// exists to complete: the checklist is the evidence audit's only input, and a
/// proposal that supplies none is refused at the moment it could still be corrected.
///
/// The oracle here is deliberately *not* "the refusal happened". It is that the
/// session holds no goal afterwards, which is what makes the remaining two steps of
/// the exploit impossible; a test that only matched the error text could not tell a
/// refusal from a refusal that still wrote the row.
#[tokio::test]
async fn a_goal_proposed_with_no_criteria_is_refused_and_no_ungated_goal_is_left_behind() {
    let fixture = Fixture::new();
    let create = erase(CreateGoalTool::new(Arc::clone(&fixture.store)));

    // The model reads the schema, not the deserializer, so the schema is where "not
    // optional" has to be visible. Pinned because a re-added `serde(default)` would
    // reopen the whole ungated path silently, and this is the one line that would
    // notice.
    let required = create.definition().parameters["required"].to_string();
    assert!(
        required.contains("success_criteria"),
        "the published schema must declare the checklist required: {required}"
    );

    let refusal = create
        .execute(
            json!({"objective": "make the parser accept trailing commas"}),
            fixture.context("call_create"),
        )
        .await
        .expect_err("a proposal with nothing to check cannot be gated, so it is not accepted");

    assert!(matches!(refusal, ToolError::InvalidArgs { .. }));
    assert!(
        refusal.is_model_correctable(),
        "the remedy is one corrected call: {refusal}"
    );
    assert_eq!(
        fixture.store.goal("ses_tools").expect("read goal"),
        None,
        "no goal row means step 2 and step 3 of the reported sequence have nothing to complete"
    );

    // Sub-variant: a checklist that is blank after trimming is not a checklist. It
    // must not become an empty one, which is the same ungated goal by another route.
    let blank = create
        .execute(
            json!({"objective": "make the parser accept trailing commas", "success_criteria": ["   "]}),
            fixture.context("call_blank"),
        )
        .await
        .expect_err("a blank criterion is not a criterion");
    assert!(matches!(blank, ToolError::InvalidArgs { .. }));
    let message = refusal_detail(&blank);
    assert!(
        message.contains("at least one non-blank success criterion"),
        "the model is told its checklist was rejected rather than silently emptied: {message}"
    );
    assert_eq!(
        fixture.store.goal("ses_tools").expect("read goal"),
        None,
        "and the blank list leaves no goal behind either"
    );
}

/// The same sequence once the proposal is corrected: with a checklist recorded, the
/// unreported `shell` edit no longer matters, because the audit reads the checklist
/// instead of the goal kind. Nothing escalates the goal here — that is the point.
#[tokio::test]
async fn a_criteria_bearing_goal_cannot_be_completed_after_an_unreported_shell_edit() {
    let fixture = Fixture::new();
    erase(CreateGoalTool::new(Arc::clone(&fixture.store)))
        .execute(
            json!({
                "objective": "make the parser accept trailing commas",
                "success_criteria": ["the parser test suite passes"]
            }),
            fixture.context("call_create"),
        )
        .await
        .expect("a proposal that says what would prove it done is accepted");

    // `shell {"command": "sed -i 's/foo/bar/' crates/zuno-parser/src/lib.rs"}` runs
    // here. It reports no written path, so the host's ledger neither escalates the
    // goal nor stamps a mutation mark: the store is called for neither, which is
    // exactly why this fixture calls nothing.
    let refusal = erase(UpdateGoalTool::new(Arc::clone(&fixture.store)))
        .execute(
            json!({"expected_revision": 1, "status": "complete"}),
            fixture.context("call_complete"),
        )
        .await
        .expect_err("the checklist is still open, whatever the goal kind says");

    assert!(matches!(refusal, ToolError::InvalidArgs { .. }));
    let message = refusal_detail(&refusal);
    assert!(message.contains("c1"), "the refusal names it: {message}");
    assert_eq!(
        fixture.store.kind("ses_tools").expect("read kind"),
        crate::GoalKind::Question,
        "no write was reported, which is the reporting gap the checklist covers"
    );
    assert_eq!(
        fixture
            .store
            .goal("ses_tools")
            .expect("read goal")
            .expect("goal exists")
            .status,
        GoalStatus::Active
    );
}

/// A change goal with no criteria is still refused, and still told where criteria come
/// from. Only [`GoalStore::create_goal`] — the user's own `/goal create` — can produce
/// such a goal now, so the remedy sentence is aimed at the run that inherited one.
#[tokio::test]
async fn a_user_created_goal_that_reported_a_write_cannot_complete_by_assertion() {
    let fixture = Fixture::new();
    fixture
        .store
        .create_goal("ses_tools", "enable structured output", None)
        .expect("the user states an objective without measuring it");

    // The host's verification ledger reports the first write; the store sees it as
    // this call.
    fixture
        .store
        .escalate_to_change("ses_tools", "`write` wrote zuno.toml", 1_000)
        .expect("escalate to a change goal");

    let refusal = erase(UpdateGoalTool::new(Arc::clone(&fixture.store)))
        .execute(
            json!({"expected_revision": 1, "status": "complete"}),
            fixture.context("call_complete"),
        )
        .await
        .expect_err("a change goal with no criteria cannot complete by assertion");
    assert!(matches!(refusal, ToolError::InvalidArgs { .. }));
    let message = refusal_detail(&refusal);
    assert!(
        message.contains("propose success criteria with `goal_propose` before completing"),
        "the model is told what would have worked: {message}"
    );
    assert_eq!(
        fixture
            .store
            .goal("ses_tools")
            .expect("read goal")
            .expect("goal exists")
            .status,
        GoalStatus::Active
    );
}

#[tokio::test]
async fn creating_a_goal_prints_the_criterion_id_to_cite_for_each_statement() {
    let fixture = Fixture::new();
    let output = erase(CreateGoalTool::new(Arc::clone(&fixture.store)))
        .execute(
            json!({
                "objective": "ship the evidence gate",
                "success_criteria": ["workspace gates pass", "release artifact exists"]
            }),
            fixture.context("call_create"),
        )
        .await
        .expect("create goal");

    assert!(
        output.output.contains("c1  workspace gates pass"),
        "the model cannot cite an id it was never shown: {}",
        output.output
    );
    assert!(
        output.output.contains("c2  release artifact exists"),
        "{}",
        output.output
    );
    let criteria = output
        .metadata
        .get("criteria")
        .expect("criteria metadata")
        .as_array()
        .expect("criteria is a list")
        .len();
    assert_eq!(criteria, 2);
    assert!(
        output.metadata.contains_key("goal"),
        "the goal metadata other callers decode stays where it was"
    );
}

#[tokio::test]
async fn a_change_goal_completes_once_every_criterion_is_cited_or_waived() {
    let fixture = Fixture::new();
    let create = erase(CreateGoalTool::new(Arc::clone(&fixture.store)));
    create
        .execute(
            json!({
                "objective": "ship the evidence gate",
                "success_criteria": ["workspace gates pass", "release artifact exists"]
            }),
            fixture.context("call_create"),
        )
        .await
        .expect("create goal");
    fixture
        .store
        .escalate_to_change("ses_tools", "edited crates/zuno-goal/src/store.rs", 1_000)
        .expect("escalate to a change goal");
    record_passing_receipt(&fixture, "rec_gates", 2_000);
    let update = erase(UpdateGoalTool::new(Arc::clone(&fixture.store)));

    let refusal = update
        .execute(
            json!({"expected_revision": 1, "status": "complete"}),
            fixture.context("call_bare_complete"),
        )
        .await
        .expect_err("prose is not evidence");
    assert!(matches!(refusal, ToolError::InvalidArgs { .. }));
    let message = refusal_detail(&refusal);
    assert!(
        message.contains("c1") && message.contains("c2"),
        "the refusal names every criterion still open: {message}"
    );

    let completed = update
        .execute(
            json!({
                "expected_revision": 1,
                "status": "complete",
                "satisfy_criteria": [{"criterionId": "c1", "receiptId": "rec_gates"}],
                "waive_criteria": [{
                    "criterionId": "c2",
                    "reason": "the artifact is built by release tooling outside this workspace"
                }]
            }),
            fixture.context("call_complete"),
        )
        .await
        .expect("citations and a waiver settle the checklist in one call");

    assert_eq!(
        goal_from_metadata(&completed)
            .expect("decode update metadata")
            .expect("goal exists")
            .status,
        GoalStatus::Complete
    );
    assert!(
        completed.output.contains("satisfied by receipt rec_gates"),
        "the result shows what the completion rests on: {}",
        completed.output
    );
    assert!(
        completed.output.contains("waived: the artifact is built"),
        "{}",
        completed.output
    );
}

#[tokio::test]
async fn citing_an_unproven_receipt_refuses_the_whole_update() {
    let fixture = Fixture::new();
    erase(CreateGoalTool::new(Arc::clone(&fixture.store)))
        .execute(
            json!({
                "objective": "ship the evidence gate",
                "success_criteria": ["workspace gates pass"]
            }),
            fixture.context("call_create"),
        )
        .await
        .expect("create goal");
    let update = erase(UpdateGoalTool::new(Arc::clone(&fixture.store)));

    let refusal = update
        .execute(
            json!({
                "expected_revision": 1,
                "status": "complete",
                "satisfy_criteria": [{"criterionId": "c1", "receiptId": "rec_imagined"}]
            }),
            fixture.context("call_invented"),
        )
        .await
        .expect_err("a receipt id the model made up proves nothing");

    assert!(matches!(refusal, ToolError::InvalidArgs { .. }));
    assert!(refusal.is_model_correctable());
    let goal = fixture
        .store
        .goal("ses_tools")
        .expect("read goal")
        .expect("goal exists");
    assert_eq!(
        goal.status,
        GoalStatus::Active,
        "the status change never runs, so the run is not silently ended"
    );
    assert_eq!(goal.revision, 1, "and nothing was written on the way out");
}

#[tokio::test]
async fn one_criterion_cannot_be_both_cited_and_waived_in_a_call() {
    let fixture = Fixture::new();
    erase(CreateGoalTool::new(Arc::clone(&fixture.store)))
        .execute(
            json!({
                "objective": "ship the evidence gate",
                "success_criteria": ["workspace gates pass"]
            }),
            fixture.context("call_create"),
        )
        .await
        .expect("create goal");
    record_passing_receipt(&fixture, "rec_gates", 2_000);

    let refusal = erase(UpdateGoalTool::new(Arc::clone(&fixture.store)))
        .execute(
            json!({
                "expected_revision": 1,
                "status": "complete",
                "satisfy_criteria": [{"criterionId": "c1", "receiptId": "rec_gates"}],
                "waive_criteria": [{"criterionId": "c1", "reason": "cannot be checked here"}]
            }),
            fixture.context("call_both"),
        )
        .await
        .expect_err("evidence and a waiver are different claims about the same criterion");

    assert!(matches!(refusal, ToolError::InvalidArgs { .. }));
    assert_eq!(
        fixture
            .store
            .criteria("ses_tools")
            .expect("read criteria")
            .first()
            .expect("one criterion")
            .status,
        crate::GoalCriterionStatus::Open,
        "the contradiction is caught before either half is applied"
    );
}

#[test]
fn the_capability_claim_description_teaches_the_provenance_contract() {
    for clause in [
        "documented",
        "probed",
        "inferred",
        "unknown",
        "probeReceiptId",
        "before configuration that relies on it is written",
        "Only documented or probed claims may be relied on",
    ] {
        assert!(
            CAPABILITY_CLAIM_DESCRIPTION.contains(clause),
            "missing `{clause}`: {CAPABILITY_CLAIM_DESCRIPTION}"
        );
    }
}

#[tokio::test]
async fn a_capability_claim_result_says_plainly_whether_the_claim_may_be_relied_on() {
    let fixture = Fixture::new();
    let tool = erase(CapabilityClaimTool::new(Arc::clone(&fixture.store)));
    assert_eq!(tool.id(), CAPABILITY_CLAIM_TOOL_ID);
    assert_eq!(tool.replay_policy(), ToolReplayPolicy::Never);
    let schema = tool.definition().parameters.to_string();
    for clause in [
        "probeReceiptId",
        "documented",
        "probed",
        "inferred",
        "unknown",
    ] {
        assert!(schema.contains(clause), "{schema}");
    }

    let inferred = tool
        .execute(
            json!({
                "capability": "bedrock:converse:structured_output",
                "subject": "vendor.model-a-v1:0",
                "state": "inferred",
                "sources": ["https://docs.example.invalid/models/model-b"]
            }),
            fixture.context("call_inferred"),
        )
        .await
        .expect("an honest guess is always recordable");
    assert_eq!(inferred.title, "Capability claim recorded");
    assert!(
        inferred.output.contains("This claim may not be relied on"),
        "{}",
        inferred.output
    );
    assert!(
        inferred.output.contains("cannot complete while it stands"),
        "{}",
        inferred.output
    );
    assert_eq!(inferred.metadata.get("reliable"), Some(&json!(false)));
    assert_eq!(inferred.metadata["capabilityClaim"]["state"], "inferred");

    let documented = tool
        .execute(
            json!({
                "capability": "bedrock:converse:structured_output",
                "subject": "vendor.model-a-v1:0",
                "state": "documented",
                "sources": ["https://docs.example.invalid/models/model-a#structured-output"]
            }),
            fixture.context("call_documented"),
        )
        .await
        .expect("a cited document");
    assert_eq!(documented.title, "Capability claim recorded");
    assert!(
        documented
            .output
            .contains("This claim may be relied on: it cites 1 source."),
        "{}",
        documented.output
    );
    assert!(
        documented
            .output
            .contains("This replaces the earlier `inferred` claim."),
        "{}",
        documented.output
    );
    assert_eq!(documented.metadata.get("reliable"), Some(&json!(true)));

    let retracted = tool
        .execute(
            json!({
                "capability": "bedrock:converse:structured_output",
                "subject": "vendor.model-a-v1:0",
                "state": "unknown"
            }),
            fixture.context("call_unknown"),
        )
        .await
        .expect("new information retracts a claim");
    assert_eq!(retracted.title, "Capability claim retracted");
    assert!(
        retracted
            .output
            .contains("This retracts the earlier `documented` claim."),
        "{}",
        retracted.output
    );
    assert_eq!(
        fixture
            .store
            .capability_claims("ses_tools")
            .expect("read claims")
            .len(),
        1,
        "three recordings of one claim are one row"
    );

    let unknown_field = tool
        .execute(
            json!({
                "capability": "bedrock:converse:structured_output",
                "subject": "vendor.model-a-v1:0",
                "state": "inferred",
                "verified": true
            }),
            fixture.context("call_extra"),
        )
        .await
        .expect_err("a field the schema does not declare is refused rather than ignored");
    assert!(matches!(unknown_field, ToolError::InvalidArgs { .. }));
}

#[tokio::test]
async fn a_probed_claim_through_the_tool_needs_the_probe_receipt_and_nothing_else_may_cite_one() {
    let fixture = Fixture::new();
    let tool = erase(CapabilityClaimTool::new(Arc::clone(&fixture.store)));
    let claim = |state: &str, receipt: Option<&str>| {
        let mut value = json!({
            "capability": "bedrock:converse:structured_output",
            "subject": "vendor.model-a-v1:0",
            "state": state,
            "sources": ["https://docs.example.invalid/models/model-a"]
        });
        if let Some(receipt) = receipt {
            value["probeReceiptId"] = json!(receipt);
        }
        value
    };

    let misplaced = tool
        .execute(
            claim("documented", Some("rec_probe")),
            fixture.context("call_misplaced"),
        )
        .await
        .expect_err("a receipt on a claim that does not rest on it would read as evidence");
    assert!(matches!(misplaced, ToolError::InvalidArgs { .. }));
    assert!(
        refusal_detail(&misplaced).contains("only valid when state is probed"),
        "{}",
        refusal_detail(&misplaced)
    );

    let uncited = tool
        .execute(claim("probed", None), fixture.context("call_uncited"))
        .await
        .expect_err("a probe nobody can cite was not observed");
    assert!(matches!(uncited, ToolError::InvalidArgs { .. }));
    assert!(uncited.is_model_correctable());
    assert!(
        refusal_detail(&uncited).contains("record it as `inferred`"),
        "{}",
        refusal_detail(&uncited)
    );

    let imagined = tool
        .execute(
            claim("probed", Some("rec_imagined")),
            fixture.context("call_imagined"),
        )
        .await
        .expect_err("a receipt id the model made up proves nothing");
    assert!(
        refusal_detail(&imagined).contains("no receipt with that id"),
        "{}",
        refusal_detail(&imagined)
    );
    assert!(
        fixture
            .store
            .capability_claims("ses_tools")
            .expect("read claims")
            .is_empty(),
        "no refusal left a row behind"
    );

    record_passing_receipt(&fixture, "rec_probe", 2_000);
    let probed = tool
        .execute(
            claim("probed", Some("rec_probe")),
            fixture.context("call_probed"),
        )
        .await
        .expect("an observed probe");
    assert!(
        probed
            .output
            .contains("This claim may be relied on: probe receipt `rec_probe` was observed"),
        "{}",
        probed.output
    );
    assert_eq!(
        probed.metadata["capabilityClaim"]["probe_receipt_id"],
        "rec_probe"
    );
    assert_eq!(probed.metadata.get("reliable"), Some(&json!(true)));
}

#[tokio::test]
async fn goal_update_cannot_complete_a_change_goal_while_a_capability_claim_is_inferred() {
    let fixture = Fixture::new();
    erase(CreateGoalTool::new(Arc::clone(&fixture.store)))
        .execute(
            json!({
                "objective": "enable structured output for model a",
                "success_criteria": ["the provider request succeeds"]
            }),
            fixture.context("call_create"),
        )
        .await
        .expect("create goal");
    fixture
        .store
        .escalate_to_change("ses_tools", "`write` wrote zuno.toml", 1_000)
        .expect("escalate to a change goal");
    record_passing_receipt(&fixture, "rec_gates", 2_000);
    erase(CapabilityClaimTool::new(Arc::clone(&fixture.store)))
        .execute(
            json!({
                "capability": "bedrock:converse:structured_output",
                "subject": "vendor.model-a-v1:0",
                "state": "inferred",
                "sources": ["https://docs.example.invalid/models/model-b"]
            }),
            fixture.context("call_claim"),
        )
        .await
        .expect("record the guess");

    let refusal = erase(UpdateGoalTool::new(Arc::clone(&fixture.store)))
        .execute(
            json!({
                "expected_revision": 1,
                "status": "complete",
                "satisfy_criteria": [{"criterionId": "c1", "receiptId": "rec_gates"}]
            }),
            fixture.context("call_complete"),
        )
        .await
        .expect_err("every criterion is proven, but the configuration rests on a guess");

    assert!(matches!(refusal, ToolError::InvalidArgs { .. }));
    assert!(refusal.is_model_correctable());
    let message = refusal_detail(&refusal);
    assert!(
        message.contains("bedrock:converse:structured_output")
            && message.contains("vendor.model-a-v1:0")
            && message.contains("only `documented` or `probed` claims may be relied on"),
        "the refusal names the reliance to settle and the states that count: {message}"
    );
    let goal = fixture
        .store
        .goal("ses_tools")
        .expect("read goal")
        .expect("goal exists");
    assert_eq!(goal.status, GoalStatus::Active);
    assert_eq!(
        goal.revision, 2,
        "the accepted citation stays; only the status change was refused"
    );
}

#[tokio::test]
async fn goal_update_refuses_to_waive_a_criterion_that_is_already_satisfied() {
    let fixture = Fixture::new();
    erase(CreateGoalTool::new(Arc::clone(&fixture.store)))
        .execute(
            json!({
                "objective": "ship the evidence gate",
                "success_criteria": ["workspace gates pass", "release artifact exists"]
            }),
            fixture.context("call_create"),
        )
        .await
        .expect("create goal");
    fixture
        .store
        .escalate_to_change("ses_tools", "edited crates/zuno-goal/src/store.rs", 1_000)
        .expect("escalate to a change goal");
    record_passing_receipt(&fixture, "rec_gates", 2_000);
    let update = erase(UpdateGoalTool::new(Arc::clone(&fixture.store)));

    // The citation lands and stays; only the completion is refused, on `c2`.
    update
        .execute(
            json!({
                "expected_revision": 1,
                "status": "complete",
                "satisfy_criteria": [{"criterionId": "c1", "receiptId": "rec_gates"}]
            }),
            fixture.context("call_cite"),
        )
        .await
        .expect_err("c2 is still open");

    let refusal = update
        .execute(
            json!({
                "expected_revision": 2,
                "status": "complete",
                "waive_criteria": [
                    {"criterionId": "c1", "reason": "second thoughts"},
                    {"criterionId": "c2", "reason": "built by release tooling"}
                ]
            }),
            fixture.context("call_waive_over_evidence"),
        )
        .await
        .expect_err("a waiver must not replace recorded evidence");

    assert!(matches!(refusal, ToolError::InvalidArgs { .. }));
    let message = refusal_detail(&refusal);
    assert!(
        message.contains("`c1` is already satisfied by receipt `rec_gates`"),
        "{message}"
    );
    let criteria = fixture.store.criteria("ses_tools").expect("read criteria");
    assert_eq!(criteria[0].status, crate::GoalCriterionStatus::Satisfied);
    assert_eq!(
        criteria[1].status,
        crate::GoalCriterionStatus::Open,
        "the refusal stops the call before the later waiver is applied"
    );
}

/// The whole sequence through the wire boundary the model actually reaches, because the
/// reviewer's reproduction was at this level: a passing receipt already on the record,
/// then `goal_propose` with a checklist, then `goal_update` citing that receipt. It used
/// to print `Ok("accepted")` with the goal at `Complete`, so the mandatory checklist
/// proved that a receipt id existed, not that anything was verified for this goal.
#[tokio::test]
async fn a_receipt_from_before_the_goal_cannot_complete_it_through_the_tools() {
    let fixture = Fixture::new();
    // No goal row yet, so this is genuinely a check that ran before the goal existed.
    record_passing_receipt(&fixture, "rec_before", 2_000);
    erase(CreateGoalTool::new(Arc::clone(&fixture.store)))
        .execute(
            json!({
                "objective": "ship the evidence gate",
                "success_criteria": ["workspace gates pass"]
            }),
            fixture.context("call_create"),
        )
        .await
        .expect("create goal");
    let update = erase(UpdateGoalTool::new(Arc::clone(&fixture.store)));

    let refusal = update
        .execute(
            json!({
                "expected_revision": 1,
                "status": "complete",
                "satisfy_criteria": [{"criterionId": "c1", "receiptId": "rec_before"}]
            }),
            fixture.context("call_launder"),
        )
        .await
        .expect_err("a check that ran before the goal existed proves nothing about it");
    assert!(matches!(refusal, ToolError::InvalidArgs { .. }));
    let message = refusal_detail(&refusal);
    assert!(
        message.contains("rec_before") && message.contains("run the check again"),
        "the refusal names the receipt and the remedy: {message}"
    );
    let goal = fixture
        .store
        .goal("ses_tools")
        .expect("read goal")
        .expect("goal exists");
    assert_eq!(
        goal.status,
        GoalStatus::Active,
        "and the run is still going, with the checklist still open"
    );
    assert_eq!(
        fixture
            .store
            .criteria("ses_tools")
            .expect("read criteria")
            .into_iter()
            .filter(|criterion| criterion.status == crate::GoalCriterionStatus::Open)
            .count(),
        1,
        "the refused update settled nothing"
    );

    // Run the check again under this goal and the same call completes.
    record_passing_receipt(&fixture, "rec_after", goal.created_at_ms + 1);
    let completed = update
        .execute(
            json!({
                "expected_revision": 1,
                "status": "complete",
                "satisfy_criteria": [{"criterionId": "c1", "receiptId": "rec_after"}]
            }),
            fixture.context("call_complete"),
        )
        .await
        .expect("a check that ran under this goal completes it");
    assert_eq!(
        goal_from_metadata(&completed)
            .expect("decode update metadata")
            .expect("goal exists")
            .status,
        GoalStatus::Complete
    );
}

/// The tool result is a model request in waiting, so the checklist it renders is bounded
/// whatever the rows hold. A 2 000 000-character waiver reason rendered a 2 000 434-byte
/// `goal_update` result, and the same reason rendered clipped in the goal document the
/// human reads — the clip existed, in one renderer only.
///
/// The assertion is on the checklist section, because it is the section
/// [`render_criterion`] owns. The JSON echo above it is the stored goal row, whose
/// `success_criteria` projection is bounded for anything this release writes
/// (`MAX_SUCCESS_CRITERIA` statements of `MAX_CRITERION_STATEMENT_CHARS`) but not for a
/// row an earlier release wrote longer.
#[tokio::test]
async fn a_checklist_result_is_bounded_however_long_a_stored_row_is() {
    let fixture = Fixture::new();
    let update = erase(UpdateGoalTool::new(Arc::clone(&fixture.store)));
    // The system path stores what an earlier release stored: no length bound at all.
    fixture
        .store
        .create_goal_with_criteria(
            "ses_tools",
            "ship it",
            &["s".repeat(2_000_000), "the gates pass".to_owned()],
            None,
        )
        .expect("the system path still accepts what an earlier release stored");
    {
        let connection = fixture.store.pool().get().expect("check out connection");
        connection
            .execute(
                "UPDATE goal_criterion SET status = 'waived', waiver_reason = ?2 \
                 WHERE session_id = ?1 AND criterion_id = 'c2'",
                rusqlite::params!["ses_tools", "w".repeat(2_000_000)],
            )
            .expect("write the waiver row a previous release accepted");
        connection
            .execute(
                "UPDATE goal_criterion SET status = 'waived', waiver_reason = 'out of scope' \
                 WHERE session_id = ?1 AND criterion_id = 'c1'",
                rusqlite::params!["ses_tools"],
            )
            .expect("settle the oversized statement too");
    }
    let revision = fixture
        .store
        .goal("ses_tools")
        .expect("read goal")
        .expect("the session has a goal")
        .revision;

    let completed = update
        .execute(
            json!({"expected_revision": revision, "status": "complete"}),
            fixture.context("call_render"),
        )
        .await
        .expect("a fully waived checklist completes");
    let checklist = completed
        .output
        .split_once("Success criteria:")
        .expect("the result renders the checklist")
        .1;
    assert!(
        checklist.len() < 2_000,
        "the checklist the model reads is bounded, not 4 MB: {} bytes",
        checklist.len()
    );
    assert!(
        checklist.contains("c1  ssss") && checklist.contains('\u{2026}'),
        "the statement identifies itself and says it was clipped: {checklist}"
    );
    assert!(
        checklist.contains("waived: www"),
        "the waiver reason renders too, clipped: {checklist}"
    );
    assert!(
        !checklist.contains(&"w".repeat(1_000)),
        "and neither one arrives whole: {} bytes",
        checklist.len()
    );
    assert_eq!(
        fixture
            .store
            .criteria("ses_tools")
            .expect("read criteria")
            .iter()
            .filter(|criterion| criterion.statement.chars().count() == 2_000_000
                || criterion
                    .waiver_reason
                    .as_deref()
                    .is_some_and(|reason| reason.chars().count() == 2_000_000))
            .count(),
        2,
        "clipping is a render bound: both stored rows are untouched"
    );
}

/// The bounds on the mandatory checklist reach the model as a corrected call, not as a
/// harness failure, and nothing is written on the way. 5000 criteria used to be accepted
/// through this exact call, and a 2 000 000-character statement with them.
#[tokio::test]
async fn goal_propose_bounds_the_checklist_it_now_requires() {
    let fixture = Fixture::new();
    let create = erase(CreateGoalTool::new(Arc::clone(&fixture.store)));

    let flood = (1..=5_000)
        .map(|index| format!("check number {index}"))
        .collect::<Vec<_>>();
    let refusal = create
        .execute(
            json!({"objective": "ship it", "success_criteria": flood}),
            fixture.context("call_flood"),
        )
        .await
        .expect_err("a checklist nobody could review is not a checklist");
    assert!(matches!(refusal, ToolError::InvalidArgs { .. }));
    let message = refusal_detail(&refusal);
    assert!(
        message.contains("5000") && message.contains("32"),
        "the refusal names what was sent and the bound in force: {message}"
    );
    assert_eq!(
        fixture.store.goal("ses_tools").expect("read goal"),
        None,
        "the bound is checked before the write"
    );

    let refusal = create
        .execute(
            json!({
                "objective": "ship it",
                "success_criteria": ["workspace gates pass", "a".repeat(2_000_000)]
            }),
            fixture.context("call_long"),
        )
        .await
        .expect_err("a statement no document could render is not a statement");
    let message = refusal_detail(&refusal);
    assert!(
        message.contains("2000000") && message.contains("500"),
        "{message}"
    );
    assert_eq!(fixture.store.goal("ses_tools").expect("read goal"), None);
}

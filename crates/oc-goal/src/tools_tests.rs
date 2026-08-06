use super::*;
use crate::GoalStatus;
use oc_tool::{AllowAll, NeverInterrupted};
use serde_json::json;
use std::sync::Arc;

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

#[tokio::test]
async fn all_three_tools_share_one_authoritative_goal() {
    let fixture = Fixture::new();
    let tools = goal_tools(Arc::clone(&fixture.store));
    assert_eq!(
        tools.iter().map(|tool| tool.id()).collect::<Vec<_>>(),
        [GET_GOAL_TOOL_ID, CREATE_GOAL_TOOL_ID, UPDATE_GOAL_TOOL_ID]
    );

    let created = tools[1]
        .execute(
            json!({"objective": "ship task 68", "token_budget": 9000}),
            fixture.context("call_create"),
        )
        .await
        .expect("create goal");
    let created_goal = goal_from_metadata(&created)
        .expect("decode create metadata")
        .expect("created goal");
    assert_eq!(created_goal.objective, "ship task 68");
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

    let completed = tools[2]
        .execute(
            json!({"status": "complete"}),
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
        .execute(json!({"status": "paused"}), fixture.context("call_paused"))
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
        .execute(json!({"objective": "first"}), fixture.context("call_first"))
        .await
        .expect("first goal");

    let conflict = create
        .execute(
            json!({"objective": "second"}),
            fixture.context("call_second"),
        )
        .await
        .expect_err("unfinished goal cannot be replaced");
    assert!(matches!(conflict, ToolError::InvalidArgs { .. }));
    assert!(conflict.is_model_correctable());

    let invalid_budget = create
        .execute(
            json!({"objective": "second", "token_budget": 0}),
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
async fn blocked_update_requires_three_persisted_matching_failure_signals() {
    let fixture = Fixture::new();
    fixture
        .store
        .create_goal("ses_tools", "enforce blocked audit", None)
        .expect("create goal");
    let tool = erase(UpdateGoalTool::new(Arc::clone(&fixture.store)));

    let early = tool
        .execute(
            json!({"status": "blocked"}),
            fixture.context("call_early_blocked"),
        )
        .await
        .expect_err("blocked before three turns must be refused");
    assert!(matches!(early, ToolError::InvalidArgs { .. }));

    for _ in 0..3 {
        fixture
            .store
            .record_failure_signal("ses_tools", Some("credential unavailable"))
            .expect("record matching blocker");
    }
    let blocked = tool
        .execute(
            json!({"status": "blocked"}),
            fixture.context("call_blocked"),
        )
        .await
        .expect("blocked after three matching turns");
    assert_eq!(
        goal_from_metadata(&blocked)
            .expect("decode metadata")
            .expect("goal exists")
            .status,
        GoalStatus::Blocked
    );
}

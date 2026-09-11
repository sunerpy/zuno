//! Native question tools share a durable port and one host capability gate.

mod support;

use std::sync::Arc;

use serde_json::json;
use support::question::{QuestionFixture, SESSION_ID, context, plan_binding};
use zuno_tool::question::QuestionPort;
use zuno_tool::{Tool, ToolEffect, ToolReplayPolicy, erase};
use zuno_tools::exposure::{ExposureFlags, exposed_conditional_tools};
use zuno_tools::invalid::InvalidTool;
use zuno_tools::plan_exit::PlanExitTool;
use zuno_tools::question::QuestionTool;
use zuno_types::question::{QuestionAction, QuestionCommand, QuestionPurpose, QuestionState};

#[test]
fn a_question_port_enables_all_question_publishers_without_an_experimental_plan_flag() {
    let unavailable = ExposureFlags::default().with_client("headless");
    let available = unavailable.clone().with_question_tool();
    for (flags, expected) in [(&unavailable, false), (&available, true)] {
        let offered = exposed_conditional_tools(flags);
        assert!(offered.contains(&"invalid"));
        for id in ["question", "question_async", "plan_exit"] {
            assert_eq!(offered.contains(&id), expected, "{id}: {flags:?}");
        }
        assert_eq!(QuestionTool::exposed_under(flags), expected);
        assert_eq!(PlanExitTool::exposed_under(flags), expected);
    }
}

#[test]
fn shared_port_tools_register_distinct_ids_and_user_mediated_non_replayable_calls() {
    let fixture = QuestionFixture::new();
    let port = fixture.shared_port();
    let registry: Vec<Arc<dyn Tool>> = vec![
        erase(InvalidTool::new()),
        erase(QuestionTool::new(Arc::clone(&port))),
        erase(QuestionTool::asynchronous(Arc::clone(&port))),
        erase(PlanExitTool::new(port)),
    ];

    let ids = registry.iter().map(|tool| tool.id()).collect::<Vec<_>>();
    assert_eq!(ids, ["invalid", "question", "question_async", "plan_exit"]);
    for tool in &registry {
        let definition = tool.definition();
        assert_eq!(definition.parameters["type"], "object");
        assert!(!definition.description.is_empty());
    }
    for tool in &registry[1..] {
        assert_eq!(tool.effect(&json!({})), ToolEffect::UserMediated);
        assert_eq!(tool.replay_policy(), ToolReplayPolicy::Never);
    }
    assert!(fixture.port.opened().is_empty());
}

#[tokio::test]
async fn a_shared_port_keeps_clarification_replies_separate_from_plan_authorization() {
    let fixture = QuestionFixture::new();
    fixture.port.bind_plan(plan_binding());
    let clarification = erase(QuestionTool::asynchronous(fixture.shared_port()));
    let approval = erase(PlanExitTool::new(fixture.shared_port()));
    let clarified = clarification
        .invoke(
            json!({"questions":[{"question":"Which database?","header":"Database"}]}),
            context("call_clarification"),
        )
        .await
        .expect("publish optional clarification");
    let confirmation = approval
        .invoke(json!({}), context("call_plan"))
        .await
        .expect("publish Plan authorization");
    let pending = fixture.port.pending(SESSION_ID).await.expect("pending");
    assert_eq!(pending.len(), 2);
    let clarification = pending
        .iter()
        .find(|view| view.purpose == QuestionPurpose::Clarification)
        .expect("clarification");
    let plan = pending
        .iter()
        .find(|view| view.purpose == QuestionPurpose::PlanAuthorization)
        .expect("Plan authorization");
    assert_eq!(
        clarified.metadata[zuno_tool::METADATA_HUMAN_REQUEST_ID_KEY],
        clarification.id
    );
    assert_eq!(
        confirmation.metadata[zuno_tool::METADATA_HUMAN_REQUEST_ID_KEY],
        plan.id
    );
    assert_ne!(clarification.id, plan.id);
    assert_eq!(fixture.port.wait_count(), 0);
    assert!(
        fixture
            .inbox()
            .pending(SESSION_ID)
            .expect("inbox")
            .is_empty()
    );

    fixture
        .port
        .apply(
            SESSION_ID,
            &clarification.id,
            QuestionCommand {
                command_id: "answer_database".to_owned(),
                expected_revision: clarification.revision,
                action: QuestionAction::Answer {
                    answers: [(
                        clarification.questions[0].id.clone(),
                        vec!["SQLite".to_owned()],
                    )]
                    .into(),
                },
            },
        )
        .await
        .expect("answer only the clarification");
    let unchanged = fixture
        .port
        .get(SESSION_ID, &plan.id)
        .await
        .expect("Plan still awaits its own explicit decision");
    assert_eq!(unchanged, *plan);
    assert_eq!(unchanged.state, QuestionState::Pending);
    assert_eq!(unchanged.decision, None);
    assert_eq!(unchanged.authorization, None);
    let inputs = fixture.inbox().pending(SESSION_ID).expect("answer inbox");
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].prompt["requestID"], clarification.id);
}

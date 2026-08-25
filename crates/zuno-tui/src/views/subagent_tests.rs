use super::*;
use crate::views::message::{Message, Role};
use crate::views::testkit::{action, press};
use crossterm::event::{KeyCode, KeyModifiers, MouseEvent, MouseEventKind};

fn message(parts: Vec<MessagePart>) -> Message {
    Message {
        role: Role::Assistant,
        id: Some("msg_1".to_owned()),
        parts,
    }
}

fn native(state: &str, job: Option<&str>) -> MessagePart {
    let job = job.map_or_else(String::new, |job| format!(" job=\"{job}\""));
    MessagePart::Tool {
        call_id: "call_native".to_owned(),
        name: "renamed_native_delegate".to_owned(),
        ui_intent: ToolUiIntent::Subagent,
        arguments: r#"{"description":"survey auth","prompt":"inspect","subagent_type":"deep","background":true}"#
            .to_owned(),
        title: None,
        status: ToolStatus::Completed,
        output: Some(format!(
            "<task id=\"ses_child\"{job} state=\"{state}\" reportDelivery=\"nextStep\">\n\
             <task_result>\nanswer\n</task_result>\n</task>"
        )),
        diff: None,
    }
}

fn product(state: &str, job: Option<&str>) -> MessagePart {
    let job = job.map_or_else(String::new, |job| format!(" job=\"{job}\""));
    MessagePart::Tool {
        call_id: "call_product".to_owned(),
        name: "company_codex".to_owned(),
        ui_intent: ToolUiIntent::Subagent,
        arguments: r#"{"description":"review patch","prompt":"review","background":true,"reportDelivery":"quiet"}"#
            .to_owned(),
        title: None,
        status: ToolStatus::Completed,
        output: Some(format!(
            "<product-agent product=\"codex\" instance=\"reviewer\" run=\"run_1\"{job} \
             state=\"{state}\" reportDelivery=\"quiet\">\n\
             <product_agent_result>\nresult\n</product_agent_result>\n</product-agent>"
        )),
        diff: None,
    }
}

fn job_observation(job: &str, status: &str) -> MessagePart {
    MessagePart::Tool {
        call_id: format!("inspect_{job}"),
        name: "job".to_owned(),
        ui_intent: ToolUiIntent::Generic,
        arguments: format!(r#"{{"jobID":"{job}"}}"#),
        title: None,
        status: ToolStatus::Completed,
        output: Some(format!(
            r#"{{"jobID":"{job}","status":"{status}","reportDelivery":"quiet","result":{{"text":"final"}},"error":null,"timeCreated":1000,"timeCompleted":3500,"subject":{{"kind":"productAgent","product":"codex","instance":"reviewer","runID":"run_1"}}}}"#
        )),
        diff: None,
    }
}

fn durable_job(status: &str, result: Option<&str>) -> zuno_types::JobProjection {
    zuno_types::JobProjection {
        id: "job_1".to_owned(),
        subject: zuno_types::JobSubjectProjection::ProductAgent {
            run_id: "run_1".to_owned(),
            product: "codex".to_owned(),
            instance: "reviewer".to_owned(),
            tool: "company_codex".to_owned(),
        },
        status: status.to_owned(),
        report_delivery: "quiet".to_owned(),
        result: result.map(str::to_owned),
        error: None,
        span: zuno_types::ExecutionSpan::from_aggregate(
            1_000,
            (status != "running").then_some(3_500),
            2_500,
            0,
            false,
        ),
        children: Vec::new(),
        time_created: 1_000,
        time_completed: (status != "running").then_some(3_500),
    }
}

fn durable_workflow(status: &str) -> zuno_types::JobProjection {
    zuno_types::JobProjection {
        id: "job_workflow".to_owned(),
        subject: zuno_types::JobSubjectProjection::Workflow {
            run_id: "run_release".to_owned(),
            workflow: "release-hardening".to_owned(),
        },
        status: status.to_owned(),
        report_delivery: "nextStep".to_owned(),
        result: None,
        error: (status == "uncertain").then(|| "runner disconnected".to_owned()),
        span: zuno_types::ExecutionSpan::from_aggregate(
            2_000,
            (status != "running").then_some(4_000),
            2_000,
            0,
            false,
        ),
        children: vec![zuno_types::JobChildProjection {
            id: "work_run_release:node:0".to_owned(),
            subject: "verify".to_owned(),
            owner: Some("fixer".to_owned()),
            status: "in_progress".to_owned(),
            span: zuno_types::ExecutionSpan::from_aggregate(2_000, None, 2_000, 0, false),
        }],
        time_created: 2_000,
        time_completed: (status != "running").then_some(4_000),
    }
}

fn durable_council(status: &str) -> zuno_types::JobProjection {
    zuno_types::JobProjection {
        id: "job_council".to_owned(),
        subject: zuno_types::JobSubjectProjection::Council {
            run_id: "run_council".to_owned(),
            preset: "balanced-review".to_owned(),
        },
        status: status.to_owned(),
        report_delivery: "nextStep".to_owned(),
        result: None,
        error: None,
        span: zuno_types::ExecutionSpan::from_aggregate(1_000, None, 8_000, 500, true),
        children: vec![
            zuno_types::JobChildProjection {
                id: "work_run_council:node:0".to_owned(),
                subject: "evidence".to_owned(),
                owner: Some("explorer".to_owned()),
                status: "completed".to_owned(),
                span: zuno_types::ExecutionSpan::from_aggregate(
                    1_000,
                    Some(4_000),
                    3_000,
                    200,
                    true,
                ),
            },
            zuno_types::JobChildProjection {
                id: "work_run_council:node:1".to_owned(),
                subject: "judgment".to_owned(),
                owner: Some("oracle".to_owned()),
                status: "in_progress".to_owned(),
                span: zuno_types::ExecutionSpan::from_aggregate(1_000, None, 5_000, 300, true),
            },
            zuno_types::JobChildProjection {
                id: "work_run_council:node:2".to_owned(),
                subject: "dissent".to_owned(),
                owner: Some("general".to_owned()),
                status: "pending".to_owned(),
                span: zuno_types::ExecutionSpan::default(),
            },
        ],
        time_created: 1_000,
        time_completed: None,
    }
}

fn joined(view: &mut SubagentView) -> String {
    view.lines(100)
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn intent_not_wire_name_selects_native_and_product_subagents() {
    let messages = vec![message(vec![
        native("completed", None),
        product("completed", None),
        MessagePart::Tool {
            call_id: "generic".to_owned(),
            name: "task".to_owned(),
            ui_intent: ToolUiIntent::Generic,
            arguments: "{}".to_owned(),
            title: None,
            status: ToolStatus::Completed,
            output: None,
            diff: None,
        },
    ])];

    let rows = delegations(&messages);
    assert_eq!(rows.len(), 2, "{rows:#?}");
    assert_eq!(rows[0].product, "zuno");
    assert_eq!(rows[0].target.as_deref(), Some("deep"));
    assert_eq!(rows[0].session_id.as_deref(), Some("ses_child"));
    assert_eq!(rows[1].product, "codex");
    assert_eq!(rows[1].target.as_deref(), Some("reviewer"));
    assert_eq!(rows[1].run_id.as_deref(), Some("run_1"));
}

#[test]
fn durable_job_output_refines_status_result_timing_and_subject() {
    for status in ["running", "completed", "failed", "cancelled", "uncertain"] {
        let messages = vec![message(vec![
            product("running", Some("job_1")),
            job_observation("job_1", status),
        ])];
        let rows = delegations(&messages);
        assert_eq!(rows[0].state, status);
        assert_eq!(rows[0].result.as_deref(), Some("final"));
        assert_eq!(rows[0].time_created, Some(1000));
        assert_eq!(rows[0].time_completed, Some(3500));
        assert_eq!(rows[0].product, "codex");
    }
}

#[test]
fn workflow_jobs_are_visible_without_forging_child_sessions() {
    let rows = delegations_with_jobs(&[], &[durable_workflow("uncertain")]);
    assert_eq!(rows.len(), 1, "{rows:#?}");
    let workflow = &rows[0];
    assert_eq!(workflow.tool, "workflow");
    assert_eq!(workflow.product, "zuno");
    assert_eq!(workflow.target.as_deref(), Some("release-hardening"));
    assert_eq!(workflow.run_id.as_deref(), Some("run_release"));
    assert!(
        workflow.session_id.is_none(),
        "workflow is not a child session"
    );
    assert_eq!(workflow.state, "uncertain");
    assert_eq!(workflow.diagnostic.as_deref(), Some("runner disconnected"));
    assert_eq!(workflow.children.len(), 1);
    assert_eq!(workflow.children[0].subject, "verify");
}

#[test]
fn council_jobs_show_typed_seat_progress_without_forging_child_sessions() {
    let rows = delegations_with_jobs(&[], &[durable_council("running")]);
    assert_eq!(rows.len(), 1, "{rows:#?}");
    let council = &rows[0];
    assert_eq!(council.tool, "council_run");
    assert_eq!(council.product, "zuno");
    assert_eq!(council.target.as_deref(), Some("balanced-review"));
    assert_eq!(council.run_id.as_deref(), Some("run_council"));
    assert!(
        council.session_id.is_none(),
        "Council is not a child session"
    );
    assert_eq!(council.children.len(), 3);

    let mut view = SubagentView::new(ViewContext::defaults(), rows);
    view.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter));
    let body = joined(&mut view);
    for expected in [
        "progress 1/3 seats done · 1 running",
        "evidence · completed · explorer",
        "judgment · in_progress · oracle",
        "durable Council seat progress",
    ] {
        assert!(body.contains(expected), "missing `{expected}`:\n{body}");
    }
}

#[test]
fn live_durable_projection_refreshes_an_open_subagent_view() {
    let state = crate::views::ambient::WorkState::new(zuno_types::WorkStateProjection {
        jobs: vec![durable_job("running", None)],
        ..zuno_types::WorkStateProjection::default()
    });
    let tasks = delegations(&[message(vec![product("running", Some("job_1"))])]);
    let mut view = SubagentView::new(ViewContext::defaults(), tasks).with_work_state(state.clone());
    view.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter));
    assert!(joined(&mut view).contains("status running"));

    state.replace(zuno_types::WorkStateProjection {
        jobs: vec![durable_job("completed", Some("durable final result"))],
        ..zuno_types::WorkStateProjection::default()
    });
    let body = joined(&mut view);
    assert!(body.contains("status completed"), "{body}");
    assert!(body.contains("result durable final result"), "{body}");
    assert!(body.contains("elapsed 2s"), "{body}");
}

#[test]
fn next_step_report_refines_a_running_row() {
    let messages = vec![
        message(vec![native("running", Some("job_native"))]),
        Message::user(
            "Background subagent `ses_child` completed job `job_native`.\n\nfinal child answer",
        ),
    ];
    let rows = delegations(&messages);
    assert_eq!(rows[0].state, "completed");
    assert_eq!(rows[0].result.as_deref(), Some("final child answer"));
}

#[test]
fn enter_opens_details_with_product_job_delivery_result_and_safety() {
    let tasks = delegations(&[message(vec![product("running", Some("job_1"))])]);
    let mut view = SubagentView::new(ViewContext::defaults(), tasks);
    assert!(!joined(&mut view).contains("safety"));

    assert_eq!(
        view.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter)),
        DialogStep::Redraw
    );
    let body = joined(&mut view);
    for expected in [
        "product codex",
        "target reviewer",
        "status running",
        "job job_1",
        "report quiet",
        "result result",
        "credentials stay outside Zuno",
    ] {
        assert!(body.contains(expected), "missing `{expected}`:\n{body}");
    }
}

#[test]
fn subagent_view_mouse_wheel_moves_the_selection() {
    let tasks = delegations(&[message(vec![
        native("running", Some("job_1")),
        product("running", Some("job_2")),
    ])]);
    let mut view = SubagentView::new(ViewContext::defaults(), tasks);
    assert_eq!(
        view.handle_mouse(
            &MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 4,
                row: 4,
                modifiers: KeyModifiers::NONE,
            },
            ratatui::layout::Rect::new(0, 0, 80, 20),
        ),
        DialogStep::Redraw
    );
    assert_eq!(view.cursor(), 1);
}

#[test]
fn x_twice_emits_cancel_and_keeps_the_list_open() {
    let tasks = delegations(&[message(vec![native("running", Some("job_1"))])]);
    let mut view = SubagentView::new(ViewContext::defaults(), tasks);
    let x = press(KeyCode::Char('x'));

    assert_eq!(
        view.handle_action(action("subagent_cancel"), &x),
        DialogStep::Redraw
    );
    assert!(joined(&mut view).contains("press x again"));
    assert_eq!(
        view.handle_action(action("subagent_cancel"), &x),
        DialogStep::Emitted(DialogOutcome::JobCancel {
            job_id: "job_1".to_owned()
        })
    );
    assert_eq!(view.len(), 1, "Emitted must not close or clear the list");
    assert_eq!(
        view.selected().map(|task| task.state.as_str()),
        Some("cancelling")
    );
}

#[test]
fn terminal_jobs_cannot_be_cancelled() {
    let tasks = delegations(&[message(vec![native("completed", Some("job_1"))])]);
    let mut view = SubagentView::new(ViewContext::defaults(), tasks);
    assert_eq!(
        view.handle_action(action("subagent_cancel"), &press(KeyCode::Char('x'))),
        DialogStep::Redraw
    );
    assert!(!joined(&mut view).contains("press x again"));
}

#[test]
fn navigation_wraps_and_escape_closes() {
    let tasks = delegations(&[message(vec![
        native("completed", None),
        product("completed", None),
    ])]);
    let mut view = SubagentView::new(ViewContext::defaults(), tasks);
    view.handle_action(action("session_child_cycle_reverse"), &press(KeyCode::Left));
    assert_eq!(view.cursor(), 1);
    assert_eq!(
        view.handle_action(action("session_parent"), &press(KeyCode::Up)),
        DialogStep::Resolved(DialogOutcome::Cancelled)
    );
}

#[test]
fn empty_and_narrow_views_are_explicit_and_safe() {
    for width in [120, 20, 1, 0] {
        let mut view = SubagentView::new(ViewContext::defaults(), Vec::new());
        assert!(view.is_empty());
        assert!(joined(&mut view).contains(EMPTY));
        let _ = view.lines(width);
        assert!(view.hints().contains(&("x x", "cancel")));
    }
}

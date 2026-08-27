#![cfg(unix)]

mod support;

use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use zuno_pty::{
    BackgroundExecutionInput, BackgroundExecutionRetention, BackgroundExecutionService,
};
use zuno_tool::{AllowAll, NeverInterrupted, ToolContext, ToolEffect, ToolReplayPolicy, TypedTool};
use zuno_tools::{BackgroundAction, BackgroundParams, BackgroundTool};

fn context(session_id: &str) -> ToolContext {
    ToolContext::new(
        session_id,
        "msg_bg",
        "call_bg",
        "build",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
}

fn input(directory: &std::path::Path, session_id: &str, command: &str) -> BackgroundExecutionInput {
    BackgroundExecutionInput {
        prepared: support::sandbox::direct_prepared(directory, command),
        session_id: session_id.to_owned(),
        title: command.to_owned(),
        command: command.to_owned(),
        hard_ceiling: Duration::from_secs(5),
        retention: BackgroundExecutionRetention::Durable,
    }
}

#[tokio::test]
async fn list_output_wait_and_cancel_share_one_execution() {
    let directory = tempfile::tempdir().expect("workspace");
    let service =
        Arc::new(BackgroundExecutionService::open(directory.path()).expect("background service"));
    let execution = service
        .start(input(
            directory.path(),
            "ses_owner",
            "printf started; sleep 30",
        ))
        .expect("background command");
    let tool = BackgroundTool::new(Arc::clone(&service));

    let listed = tool
        .run(
            BackgroundParams {
                action: BackgroundAction::List,
                task_id: None,
                cursor: None,
                timeout: None,
            },
            context("ses_owner"),
        )
        .await
        .expect("list");
    assert!(listed.output.contains(execution.id.as_str()));

    let hidden = tool
        .run(
            BackgroundParams {
                action: BackgroundAction::List,
                task_id: None,
                cursor: None,
                timeout: None,
            },
            context("ses_other"),
        )
        .await
        .expect("other session list");
    assert!(!hidden.output.contains(execution.id.as_str()));

    let checkpoint = tool
        .run(
            BackgroundParams {
                action: BackgroundAction::Wait,
                task_id: Some(execution.id.as_str().to_owned()),
                cursor: None,
                timeout: Some(20),
            },
            context("ses_owner"),
        )
        .await
        .expect("wait checkpoint");
    assert!(checkpoint.output.contains("\"waitTimedOut\": true"));
    assert!(checkpoint.output.contains("started"));

    let cancelled = tool
        .run(
            BackgroundParams {
                action: BackgroundAction::Cancel,
                task_id: Some(execution.id.as_str().to_owned()),
                cursor: None,
                timeout: None,
            },
            context("ses_owner"),
        )
        .await
        .expect("cancel");
    assert!(cancelled.output.contains("\"cancellationRequested\": true"));

    let settled = service
        .wait(&execution.id, None)
        .await
        .expect("settled after cancellation");
    assert_eq!(settled.info.status.as_str(), "cancelled");
}

#[tokio::test]
async fn another_session_cannot_inspect_or_cancel_an_execution() {
    let directory = tempfile::tempdir().expect("workspace");
    let service =
        Arc::new(BackgroundExecutionService::open(directory.path()).expect("background service"));
    let execution = service
        .start(input(directory.path(), "ses_owner", "sleep 30"))
        .expect("background command");
    let tool = BackgroundTool::new(Arc::clone(&service));

    let error = tool
        .run(
            BackgroundParams {
                action: BackgroundAction::Output,
                task_id: Some(execution.id.as_str().to_owned()),
                cursor: None,
                timeout: None,
            },
            context("ses_other"),
        )
        .await
        .expect_err("cross-session inspection is rejected");
    assert!(
        zuno_error::source::describe(&error).contains("not found for this session"),
        "{error:?}"
    );

    service.cancel(&execution.id).expect("cleanup");
    service
        .wait(&execution.id, None)
        .await
        .expect("cleanup settles");
}

#[test]
fn mixed_read_and_cancel_surface_is_never_replayable() {
    let directory = tempfile::tempdir().expect("workspace");
    let service =
        Arc::new(BackgroundExecutionService::open(directory.path()).expect("background service"));
    assert_eq!(
        BackgroundTool::new(service).replay_policy(),
        ToolReplayPolicy::Never
    );
}

#[test]
fn strict_effect_is_dynamic_for_background_actions() {
    let directory = tempfile::tempdir().expect("workspace");
    let service =
        Arc::new(BackgroundExecutionService::open(directory.path()).expect("background service"));
    let tool = BackgroundTool::new(service);
    for action in ["list", "output", "wait"] {
        assert_eq!(
            tool.effect(&json!({"action": action})),
            ToolEffect::ReadOnly,
            "{action}"
        );
    }
    assert_eq!(
        tool.effect(&json!({"action": "cancel"})),
        ToolEffect::SideEffecting
    );
}

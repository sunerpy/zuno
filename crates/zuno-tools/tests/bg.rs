#![cfg(unix)]

mod support;

use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use zuno_db::completion_delivery::{CompletionDeliveryStore, CompletionOwner};
use zuno_db::inbox::{SessionInbox, SubmissionState};
use zuno_db::session_execution::SessionExecutionStore;
use zuno_db::{Pool, migration, session};
use zuno_paths::DbLocation;
use zuno_pty::{
    BackgroundExecutionId, BackgroundExecutionInput, BackgroundExecutionPurpose,
    BackgroundExecutionRetention, BackgroundExecutionService,
};
use zuno_tool::{
    AllowAll, NeverInterrupted, ToolContext, ToolEffect, ToolOutputStore, ToolReplayPolicy,
    TypedTool,
};
use zuno_tools::{BackgroundAction, BackgroundParams, BackgroundTool};
use zuno_types::execution::{CollaborationMode, SessionExecutionPhase};

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
        cycle_id: None,
        title: command.to_owned(),
        command: command.to_owned(),
        purpose: BackgroundExecutionPurpose::Command,
        hard_ceiling: Duration::from_secs(5),
        retention: BackgroundExecutionRetention::Durable,
    }
}

fn completion_pool() -> Arc<Pool> {
    let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("pool"));
    {
        let mut connection = pool.get().expect("connection");
        migration::apply(&mut connection).expect("schema");
        connection
            .execute(
                "INSERT INTO project \
                 (id, worktree, time_created, time_updated, sandboxes) \
                 VALUES ('project', '/workspace', 1, 1, '[]')",
                [],
            )
            .expect("project");
    }
    pool.transaction(|transaction| {
        session::create(
            transaction,
            &session::SessionCreate::new(
                "ses_owner",
                "background",
                "project",
                "/workspace",
                "/workspace",
                "Background completion",
                "zuno",
            )
            .at(1),
        )
        .map(|_| ())
    })
    .expect("session");
    pool
}

fn read_params(action: BackgroundAction, id: &BackgroundExecutionId) -> BackgroundParams {
    BackgroundParams {
        action,
        task_id: Some(id.as_str().to_owned()),
        cursor: None,
        limit: None,
        timeout: matches!(action, BackgroundAction::Wait).then_some(10),
        output_path: None,
    }
}

fn shell_params(background: bool) -> zuno_tools::shell::ShellParams {
    zuno_tools::shell::ShellParams {
        command: "while [ ! -f release ]; do sleep 0.01; done; printf origin-result".to_owned(),
        timeout: Some(10),
        workdir: None,
        background,
        background_purpose: BackgroundExecutionPurpose::RemoteObserver,
        expected_git_head: None,
        exit_policy: None,
    }
}

fn turn_context() -> ToolContext {
    let snapshot = serde_json::from_value(json!({
        "schemaVersion": 4,
        "turnId": "turn_distinct_from_work_cycle",
        "step": 1,
        "capability": {
            "schemaVersion": 4,
            "pack": {"id":"test","version":"1","upstreamRevision":"test"},
            "extensionRevision": 0,
            "permissionPolicySha256": "policy",
            "sandbox": {
                "mode": "workspace-write", "network": "deny",
                "writableRoots": [], "protectedPaths": []
            },
            "profiles": [], "presets": [], "councils": [], "workflows": [], "skills": []
        },
        "owner": {
            "sessionId":"ses_owner", "parentSessionId":null, "parentAttempt":null,
            "workflow":null, "workflowNode":null
        },
        "agent": {
            "name":"build", "sourceId":"test://build", "definitionSha256":"definition",
            "permissionSha256":"permission", "promptPolicySha256":"prompt"
        },
        "model": {
            "providerId":"fake", "modelId":"fake-model", "wireModelId":"fake-model",
            "surface":"responses", "reasoningSha256":"reasoning", "preset":null
        },
        "selectedSkills": [],
        "prompt": {"eventId":"evt-parent","assemblySha256":"assembly","actualSha256":"actual"},
        "tools": []
    }))
    .expect("attempt with a turn id distinct from the work cycle");
    context("ses_owner").with_orchestration_snapshot(Arc::new(snapshot))
}

fn set_cycle(store: &SessionExecutionStore, cycle_id: &str) {
    let mut state = store
        .seed("ses_owner", CollaborationMode::Work, None, 10)
        .expect("execution state");
    state.cycle_id = Some(cycle_id.to_owned());
    state.phase = SessionExecutionPhase::Running;
    state.time_updated += 1;
    store.update(state.revision, state).expect("set cycle");
}

#[tokio::test]
async fn a_lost_execution_handle_is_uncertain_and_cannot_spawn_a_replacement() {
    let directory = tempfile::tempdir().expect("process state");
    let service = Arc::new(BackgroundExecutionService::open(directory.path()).expect("service"));
    let missing = BackgroundExecutionId::parse("bg_0123456789abcdef0123456789abcdef").expect("id");
    for action in [BackgroundAction::Wait, BackgroundAction::Output] {
        let error = BackgroundTool::new(Arc::clone(&service))
            .run(read_params(action, &missing), context("ses_owner"))
            .await
            .expect_err("lost handle");
        assert!(matches!(error, zuno_error::ToolError::Uncertain { .. }));
    }
    assert!(service.list().is_empty());
    assert!(service.foreground_for_session("ses_owner").is_empty());
    assert_eq!(
        std::fs::read_dir(directory.path())
            .expect("state directory")
            .count(),
        0
    );
}

#[tokio::test]
async fn a_failed_durable_foreground_receipt_leaves_the_original_handle_pending() {
    let directory = tempfile::tempdir().expect("workspace");
    let background = tempfile::tempdir().expect("process state");
    let service = Arc::new(BackgroundExecutionService::open(background.path()).expect("service"));
    let mut callbacks = service.subscribe();
    let shell = support::sandbox::shell_tool(directory.path())
        .with_background_executions(Arc::clone(&service));
    let launched = shell
        .run(shell_params(false), context("ses_owner"))
        .await
        .expect("foreground");
    let id = BackgroundExecutionId::parse(launched.metadata["task_id"].as_str().expect("id"))
        .expect("valid");
    std::fs::write(directory.path().join("release"), b"finish").expect("release");
    service.wait(&id, None).await.expect("settled");
    let missing_schema = Arc::new(Pool::open(&DbLocation::Memory).expect("empty test DB"));
    let tool = BackgroundTool::new(Arc::clone(&service)).with_completion_delivery(missing_schema);
    assert!(
        tool.run(
            read_params(BackgroundAction::Output, &id),
            context("ses_owner")
        )
        .await
        .is_err()
    );
    assert_eq!(service.foreground_for_session("ses_owner").len(), 1);
    assert!(
        !service
            .foreground(&id, "ses_owner")
            .expect("original handle")
            .consumed
    );
    assert_eq!(
        service.complete_output(&id).expect("recoverable output"),
        b"origin-result"
    );
    assert!(callbacks.try_recv().is_err());

    let pool = completion_pool();
    let output = BackgroundTool::new(Arc::clone(&service))
        .with_completion_delivery(Arc::clone(&pool))
        .run(
            read_params(BackgroundAction::Wait, &id),
            context("ses_owner"),
        )
        .await
        .expect("recover delivery");
    assert_eq!(
        output.metadata[zuno_tools::bg::BACKGROUND_METADATA_KEY]["completionClaimedInline"],
        true
    );
    assert!(service.foreground_for_session("ses_owner").is_empty());
    assert!(
        SessionInbox::new(pool)
            .pending("ses_owner")
            .expect("inbox")
            .is_empty()
    );
}

#[tokio::test]
async fn corrupt_foreground_verification_is_not_reconstructed_or_consumed() {
    let directory = tempfile::tempdir().expect("workspace");
    let background = tempfile::tempdir().expect("process state");
    let service = Arc::new(BackgroundExecutionService::open(background.path()).expect("service"));
    let shell = support::sandbox::shell_tool(directory.path())
        .with_background_executions(Arc::clone(&service));
    let launched = shell
        .run(shell_params(false), context("ses_owner"))
        .await
        .expect("foreground");
    let id = BackgroundExecutionId::parse(launched.metadata["task_id"].as_str().expect("id"))
        .expect("valid");
    std::fs::write(directory.path().join("release"), b"finish").expect("release");
    let settled = service.wait(&id, None).await.expect("settled").info;
    let mut row: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&settled.status_file).expect("row")).expect("JSON");
    row["foreground"]["context"]["metadata"]["shell"]["version"] = json!(99);
    std::fs::write(&settled.status_file, serde_json::to_vec(&row).expect("row"))
        .expect("corrupt fixture");
    let restored = Arc::new(BackgroundExecutionService::open(background.path()).expect("reopen"));
    let pool = completion_pool();
    let error = BackgroundTool::new(Arc::clone(&restored))
        .with_completion_delivery(Arc::clone(&pool))
        .run(
            read_params(BackgroundAction::Output, &id),
            context("ses_owner"),
        )
        .await
        .expect_err("unknown receipt version");
    assert!(matches!(error, zuno_error::ToolError::Uncertain { .. }));
    assert!(
        !restored
            .foreground(&id, "ses_owner")
            .expect("pending handle")
            .consumed
    );
    assert!(
        CompletionDeliveryStore::new(pool)
            .get(&zuno_tools::bg::background_completion_source_key(&settled))
            .expect("lookup")
            .is_none()
    );
}

#[tokio::test]
async fn running_output_and_wait_do_not_publish_or_claim_a_completion() {
    let directory = tempfile::tempdir().expect("workspace");
    let service =
        Arc::new(BackgroundExecutionService::open(directory.path()).expect("background service"));
    let pool = completion_pool();
    let delivery = CompletionDeliveryStore::new(Arc::clone(&pool));
    let tool =
        BackgroundTool::new(Arc::clone(&service)).with_completion_delivery(Arc::clone(&pool));
    let mut launch = input(directory.path(), "ses_owner", "sleep 30");
    launch.hard_ceiling = Duration::from_secs(30);
    let execution = service.start(launch).expect("running command");
    for action in [BackgroundAction::Output, BackgroundAction::Wait] {
        let output = tool
            .run(read_params(action, &execution.id), context("ses_owner"))
            .await
            .expect("running read");
        let facts = &output.metadata[zuno_tools::bg::BACKGROUND_METADATA_KEY];
        assert_eq!(facts["execution"]["status"], "running");
        assert_eq!(facts["completionClaimedInline"], false);
        assert!(
            delivery
                .get(&zuno_tools::bg::background_completion_source_key(
                    &execution
                ))
                .expect("completion lookup")
                .is_none()
        );
        assert!(
            delivery
                .unclaimed_for_session("ses_owner")
                .expect("completions")
                .is_empty()
        );
        assert!(
            SessionInbox::new(Arc::clone(&pool))
                .pending("ses_owner")
                .expect("pending callbacks")
                .is_empty()
        );
    }
    service.cancel(&execution.id).expect("cancel command");
    service
        .wait(&execution.id, None)
        .await
        .expect("cancel settles");
    let output = tool
        .run(
            read_params(BackgroundAction::Output, &execution.id),
            context("ses_owner"),
        )
        .await
        .expect("terminal cancellation read");
    let facts = &output.metadata[zuno_tools::bg::BACKGROUND_METADATA_KEY];
    assert_eq!(facts["execution"]["status"], "cancelled");
    assert_eq!(facts["completionClaimedInline"], true);
}

#[tokio::test]
async fn terminal_output_and_wait_claim_once_in_either_order_before_callback() {
    for first in [BackgroundAction::Output, BackgroundAction::Wait] {
        for command in ["printf terminal-result", "printf terminal-result; exit 7"] {
            let directory = tempfile::tempdir().expect("workspace");
            let service = Arc::new(
                BackgroundExecutionService::open(directory.path()).expect("background service"),
            );
            let pool = completion_pool();
            let delivery = CompletionDeliveryStore::new(Arc::clone(&pool));
            let tool = BackgroundTool::new(Arc::clone(&service))
                .with_completion_delivery(Arc::clone(&pool));
            let execution = service
                .start(input(directory.path(), "ses_owner", command))
                .expect("background command");
            let settled = service
                .wait(&execution.id, None)
                .await
                .expect("command settles")
                .info;
            let second = match first {
                BackgroundAction::Output => BackgroundAction::Wait,
                _ => BackgroundAction::Output,
            };
            for (action, expected) in [(first, true), (second, false), (first, false)] {
                let output = tool
                    .run(read_params(action, &execution.id), context("ses_owner"))
                    .await
                    .expect("terminal read");
                let facts = &output.metadata[zuno_tools::bg::BACKGROUND_METADATA_KEY];
                assert_eq!(facts["execution"]["status"], settled.status.as_str());
                assert_eq!(facts["output"], "terminal-result");
                assert_eq!(facts["completionClaimedInline"], expected);
            }
            let source_key = zuno_tools::bg::background_completion_source_key(&settled);
            assert_eq!(
                delivery
                    .get(&source_key)
                    .expect("delivery")
                    .expect("published completion")
                    .owner,
                Some(CompletionOwner::Inline)
            );
            assert!(
                delivery
                    .claim_callback(
                        &source_key,
                        zuno_tools::bg::background_completion_input(&settled),
                        settled.time_updated,
                    )
                    .expect("late callback")
                    .is_none()
            );
            assert!(
                SessionInbox::new(pool)
                    .pending("ses_owner")
                    .expect("no callback admission")
                    .is_empty()
            );
        }
    }
}

#[tokio::test]
async fn terminal_reads_supersede_pending_callbacks_but_preserve_promoted_and_consumed_inputs() {
    for action in [BackgroundAction::Output, BackgroundAction::Wait] {
        for state in [
            SubmissionState::Steering,
            SubmissionState::Promoted,
            SubmissionState::Consumed,
        ] {
            let directory = tempfile::tempdir().expect("workspace");
            let service = Arc::new(
                BackgroundExecutionService::open(directory.path()).expect("background service"),
            );
            let pool = completion_pool();
            let delivery = CompletionDeliveryStore::new(Arc::clone(&pool));
            let inbox = SessionInbox::new(Arc::clone(&pool));
            set_cycle(
                &SessionExecutionStore::new(Arc::clone(&pool)),
                "cycle_bg_consumption",
            );
            let tool = BackgroundTool::new(Arc::clone(&service)).with_completion_delivery(pool);
            let mut launch = input(directory.path(), "ses_owner", "printf terminal-result");
            launch.cycle_id = Some("cycle_bg_consumption".to_owned());
            let execution = service.start(launch).expect("background command");
            let settled = service
                .wait(&execution.id, None)
                .await
                .expect("command settles")
                .info;
            let envelope = zuno_tools::bg::background_completion_envelope(&settled);
            delivery
                .publish(envelope.clone(), settled.time_updated)
                .expect("publish");
            let (_, input) = delivery
                .claim_callback(
                    &envelope.source_key,
                    zuno_tools::bg::background_completion_input(&settled),
                    settled.time_updated,
                )
                .expect("callback claim")
                .expect("pending callback");
            let mut before = input.clone();
            if !state.is_pending() {
                before = inbox
                    .promote_revision("ses_owner", &input.id, input.revision)
                    .expect("promote callback")
                    .expect("reserved callback");
            }
            if state == SubmissionState::Consumed {
                before = inbox
                    .mark_consumed("ses_owner", &input.id)
                    .expect("consume callback")
                    .expect("model-visible callback");
            }
            let output = tool
                .run(read_params(action, &execution.id), context("ses_owner"))
                .await
                .expect("explicit terminal read");
            let facts = &output.metadata[zuno_tools::bg::BACKGROUND_METADATA_KEY];
            assert_eq!(facts["output"], "terminal-result");
            assert_eq!(facts["completionClaimedInline"], state.is_pending());
            let after = inbox
                .get("ses_owner", &input.id)
                .expect("callback audit")
                .expect("retained input");
            let completion = delivery
                .get(&envelope.source_key)
                .expect("delivery")
                .expect("published completion");
            if state.is_pending() {
                assert_eq!(after.state, SubmissionState::Cancelled);
                assert_eq!(after.revision, input.revision + 1);
                assert_eq!(after.prompt, input.prompt);
                assert_eq!(completion.owner, Some(CompletionOwner::Inline));
                assert_eq!(completion.input_id, None);
                assert!(
                    inbox
                        .promote_revision("ses_owner", &input.id, input.revision)
                        .expect("stale queued steer")
                        .is_none()
                );
            } else {
                assert_eq!(after, before);
                assert_eq!(completion.owner, Some(CompletionOwner::Callback));
                assert_eq!(completion.input_id.as_deref(), Some(input.id.as_str()));
            }
            let repeated = tool
                .run(read_params(action, &execution.id), context("ses_owner"))
                .await
                .expect("repeat terminal inspection");
            assert_eq!(
                repeated.metadata[zuno_tools::bg::BACKGROUND_METADATA_KEY]["completionClaimedInline"],
                false
            );
        }
    }
}

#[tokio::test]
async fn shell_preserves_the_origin_cycle_across_later_cycles_for_background_and_yielded_runs() {
    for background in [true, false] {
        let directory = tempfile::tempdir().expect("workspace");
        let background_dir = tempfile::tempdir().expect("background directory");
        let service = Arc::new(
            BackgroundExecutionService::open(background_dir.path()).expect("background service"),
        );
        let pool = completion_pool();
        let execution_store = SessionExecutionStore::new(Arc::clone(&pool));
        set_cycle(&execution_store, "cycle_origin");
        let shell = support::sandbox::shell_tool(directory.path())
            .with_background_executions(Arc::clone(&service))
            .with_execution_store(Arc::clone(&pool));
        let ctx = turn_context();
        assert_ne!(
            ctx.orchestration_snapshot().expect("attempt").turn_id,
            "cycle_origin"
        );
        let launched = shell
            .run(shell_params(background), ctx)
            .await
            .expect("launch shell background command");
        let id = BackgroundExecutionId::parse(
            launched.metadata["task_id"]
                .as_str()
                .expect("background id"),
        )
        .expect("valid id");
        let running = service.get(&id).expect("running execution");
        assert_eq!(running.cycle_id.as_deref(), Some("cycle_origin"));
        assert_eq!(running.status.as_str(), "running");
        if !background {
            assert_eq!(launched.metadata["foreground_yielded"], true);
        }
        set_cycle(&execution_store, "cycle_later_before_completion");
        std::fs::write(directory.path().join("release"), b"finish").expect("release command");
        let settled = service
            .wait(&id, Some(Duration::from_secs(3)))
            .await
            .expect("command settles")
            .info;
        assert_eq!(settled.status.as_str(), "completed");
        assert_eq!(settled.cycle_id.as_deref(), Some("cycle_origin"));
        if !background {
            set_cycle(&execution_store, "cycle_later_before_inline_read");
            let restored = Arc::new(
                BackgroundExecutionService::open(background_dir.path())
                    .expect("restored foreground"),
            );
            let foreground = restored
                .foreground(&id, "ses_owner")
                .expect("original handle");
            assert!(restored.list_for_session("ses_owner").is_empty());
            let tool = BackgroundTool::new(Arc::clone(&restored))
                .with_completion_delivery(Arc::clone(&pool));
            let output = tool
                .run(read_params(BackgroundAction::Output, &id), turn_context())
                .await
                .expect("inline foreground completion");
            let receipt = zuno_tool::VerificationReceipt::from_metadata(&output.metadata)
                .expect("valid receipt")
                .expect("foreground receipt");
            assert!(
                !receipt.proves_success(),
                "a remote observer cannot prove remote success"
            );
            let expected = zuno_tools::bg::foreground_completion_envelope(&foreground, &receipt);
            let delivery = CompletionDeliveryStore::new(Arc::clone(&pool));
            let stored = delivery
                .get(&expected.source_key)
                .expect("lookup")
                .expect("stored receipt");
            assert_eq!(stored.envelope, expected);
            assert_eq!(stored.owner, Some(CompletionOwner::Inline));
            assert_eq!(stored.envelope.cycle_id.as_deref(), Some("cycle_origin"));
            assert_eq!(stored.envelope.payload["originCallID"], "call_bg");
            assert!(
                delivery
                    .unclaimed_for_session("ses_owner")
                    .expect("unclaimed")
                    .is_empty()
            );
            assert!(
                SessionInbox::new(Arc::clone(&pool))
                    .pending("ses_owner")
                    .expect("inbox")
                    .is_empty()
            );
            assert!(restored.foreground_for_session("ses_owner").is_empty());
            let repeated = tool
                .run(read_params(BackgroundAction::Wait, &id), turn_context())
                .await
                .expect("repeat observation");
            assert_eq!(
                repeated.metadata[zuno_tools::bg::BACKGROUND_METADATA_KEY]["completionClaimedInline"],
                false
            );
            assert_eq!(
                execution_store
                    .get("ses_owner")
                    .expect("state")
                    .expect("execution")
                    .cycle_id
                    .as_deref(),
                Some("cycle_later_before_inline_read")
            );
            continue;
        }
        let envelope = zuno_tools::bg::background_completion_envelope(&settled);
        let callback = zuno_tools::bg::background_completion_input(&settled);
        assert_eq!(envelope.cycle_id.as_deref(), Some("cycle_origin"));
        assert_eq!(callback.cycle_id, envelope.cycle_id);
        let delivery = CompletionDeliveryStore::new(Arc::clone(&pool));
        delivery
            .publish(envelope.clone(), settled.time_updated)
            .expect("publish origin completion");
        let (_, admitted) = delivery
            .claim_callback(&envelope.source_key, callback, settled.time_updated)
            .expect("claim callback")
            .expect("pending callback");
        assert_eq!(admitted.cycle_id.as_deref(), Some("cycle_origin"));
        set_cycle(&execution_store, "cycle_later_before_inline_read");

        let restored = Arc::new(
            BackgroundExecutionService::open(background_dir.path())
                .expect("reopen terminal metadata"),
        );
        let output = BackgroundTool::new(restored)
            .with_completion_delivery(Arc::clone(&pool))
            .run(read_params(BackgroundAction::Output, &id), turn_context())
            .await
            .expect("consume restored origin completion");
        let facts = &output.metadata[zuno_tools::bg::BACKGROUND_METADATA_KEY];
        assert_eq!(facts["completionClaimedInline"], true);
        assert_eq!(facts["execution"]["cycleID"], "cycle_origin");
        assert_eq!(facts["output"], "origin-result");
        let completion = delivery
            .get(&envelope.source_key)
            .expect("completion")
            .expect("retained completion");
        assert_eq!(completion.envelope, envelope);
        assert_eq!(completion.owner, Some(CompletionOwner::Inline));
        assert_eq!(
            execution_store
                .get("ses_owner")
                .expect("current state")
                .expect("execution state")
                .cycle_id
                .as_deref(),
            Some("cycle_later_before_inline_read")
        );
        let audit = SessionInbox::new(pool)
            .get("ses_owner", &admitted.id)
            .expect("callback audit")
            .expect("cancelled callback retained");
        assert_eq!(audit.state, SubmissionState::Cancelled);
        assert_eq!(audit.cycle_id.as_deref(), Some("cycle_origin"));
    }
}

#[tokio::test]
async fn shell_without_an_origin_cycle_and_legacy_metadata_stay_inspectable_and_unbound() {
    for configure_store in [false, true] {
        let directory = tempfile::tempdir().expect("workspace");
        let background_dir = tempfile::tempdir().expect("background directory");
        let service = Arc::new(
            BackgroundExecutionService::open(background_dir.path()).expect("background service"),
        );
        let pool = completion_pool();
        let execution_store = SessionExecutionStore::new(Arc::clone(&pool));
        let mut shell = support::sandbox::shell_tool(directory.path())
            .with_background_executions(Arc::clone(&service));
        if configure_store {
            shell = shell.with_execution_store(Arc::clone(&pool));
        } else {
            set_cycle(&execution_store, "cycle_without_configured_capture");
        }
        let launched = shell
            .run(shell_params(true), turn_context())
            .await
            .expect("launch without origin binding");
        let id = BackgroundExecutionId::parse(
            launched.metadata["task_id"]
                .as_str()
                .expect("background id"),
        )
        .expect("valid id");
        assert_eq!(service.get(&id).expect("execution").cycle_id, None);
        set_cycle(&execution_store, "cycle_newer_than_legacy_command");
        std::fs::write(directory.path().join("release"), b"finish").expect("release command");
        let settled = service
            .wait(&id, Some(Duration::from_secs(3)))
            .await
            .expect("command settles")
            .info;
        assert_eq!(settled.status.as_str(), "completed");
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&settled.status_file).expect("status metadata"))
                .expect("status JSON");
        assert!(
            persisted["info"].get("cycleId").is_none(),
            "this is the older metadata shape with no cycle field"
        );
        let reopened = Arc::new(
            BackgroundExecutionService::open(background_dir.path())
                .expect("legacy metadata readable"),
        );
        let legacy = reopened.get(&id).expect("legacy execution");
        assert_eq!(legacy.cycle_id, None);
        assert_eq!(
            zuno_tools::bg::background_completion_input(&legacy).cycle_id,
            None
        );
        let output = BackgroundTool::new(reopened)
            .with_completion_delivery(Arc::clone(&pool))
            .run(read_params(BackgroundAction::Output, &id), turn_context())
            .await
            .expect("legacy output remains inspectable");
        let facts = &output.metadata[zuno_tools::bg::BACKGROUND_METADATA_KEY];
        assert_eq!(facts["output"], "origin-result");
        assert_eq!(facts["execution"]["cycleID"], serde_json::Value::Null);
        assert_eq!(facts["completionClaimedInline"], true);
        assert_eq!(
            CompletionDeliveryStore::new(pool)
                .get(&zuno_tools::bg::background_completion_source_key(&legacy))
                .expect("legacy completion")
                .expect("retained completion")
                .envelope
                .cycle_id,
            None
        );
    }
}

#[tokio::test]
async fn failure_to_read_the_origin_cycle_prevents_the_shell_spawn() {
    let directory = tempfile::tempdir().expect("workspace");
    let background_dir = tempfile::tempdir().expect("background directory");
    let service = Arc::new(
        BackgroundExecutionService::open(background_dir.path()).expect("background service"),
    );
    let missing_schema = Arc::new(Pool::open(&DbLocation::Memory).expect("uninitialized pool"));
    let shell = support::sandbox::shell_tool(directory.path())
        .with_background_executions(Arc::clone(&service))
        .with_execution_store(missing_schema);
    let mut params = shell_params(true);
    params.command = "touch must-not-spawn".to_owned();
    assert!(shell.run(params, turn_context()).await.is_err());
    assert!(service.list().is_empty());
    assert!(!directory.path().join("must-not-spawn").exists());
}

#[tokio::test]
async fn list_output_wait_and_cancel_share_one_execution() {
    let directory = tempfile::tempdir().expect("workspace");
    let service =
        Arc::new(BackgroundExecutionService::open(directory.path()).expect("background service"));
    let mut observer = input(directory.path(), "ses_owner", "printf started; sleep 30");
    observer.purpose = BackgroundExecutionPurpose::RemoteObserver;
    let execution = service.start(observer).expect("background command");
    let tool = BackgroundTool::new(Arc::clone(&service));

    let listed = tool
        .run(
            BackgroundParams {
                action: BackgroundAction::List,
                task_id: None,
                cursor: None,
                limit: None,
                timeout: None,
                output_path: None,
            },
            context("ses_owner"),
        )
        .await
        .expect("list");
    assert!(listed.output.contains(execution.id.as_str()));
    assert!(listed.output.contains("\"purpose\": \"remoteObserver\""));
    assert!(
        listed
            .output
            .contains("\"requiresAuthoritativeRefresh\": true")
    );

    let hidden = tool
        .run(
            BackgroundParams {
                action: BackgroundAction::List,
                task_id: None,
                cursor: None,
                limit: None,
                timeout: None,
                output_path: None,
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
                limit: None,
                timeout: Some(20),
                output_path: None,
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
                limit: None,
                timeout: None,
                output_path: None,
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
                limit: None,
                timeout: None,
                output_path: None,
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
    for action in ["list", "output", "wait", "artifact"] {
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

/// A read that names no cursor gets the newest window, which is where a command reports.
///
/// This is the call shape the tool description teaches and the one a model makes while
/// watching a build: no `cursor`, no `limit`. Serving it from the oldest retained bytes
/// returned the same opening lines to every poll, put the failing assertion and the
/// summary roughly 24 paging calls away, and left `shell` with `tail` the cheapest way to
/// see what a command had just said — the incentive this tool exists to remove.
#[tokio::test]
async fn a_read_that_names_no_cursor_returns_the_newest_window() {
    let directory = tempfile::tempdir().expect("workspace");
    let service =
        Arc::new(BackgroundExecutionService::open(directory.path()).expect("background service"));
    let filler = zuno_tools::bg::DEFAULT_WINDOW_BYTES + 4_096;
    let execution = service
        .start(input(
            directory.path(),
            "ses_owner",
            &format!("printf 'OPENING LINE\\n'; head -c {filler} /dev/zero | tr '\\0' x; printf '\\nFAILED: 1 test\\n'"),
        ))
        .expect("background command");
    service.wait(&execution.id, None).await.expect("settles");
    let tool = BackgroundTool::new(Arc::clone(&service));

    let window = tool
        .run(
            BackgroundParams {
                action: BackgroundAction::Output,
                task_id: Some(execution.id.as_str().to_owned()),
                cursor: None,
                limit: None,
                timeout: None,
                output_path: None,
            },
            context("ses_owner"),
        )
        .await
        .expect("newest window");

    let facts = &window.metadata[zuno_tools::bg::BACKGROUND_METADATA_KEY];
    let text = facts["output"].as_str().expect("window text");
    assert!(text.contains("FAILED: 1 test"), "the tail has to be in it");
    assert!(
        !text.contains("OPENING LINE"),
        "a window this size cannot also hold the head"
    );
    assert_eq!(
        facts["cursor"], facts["totalWritten"],
        "nothing newer remains, so paging forward is finished"
    );
    assert_eq!(facts["hasMore"], false);
    assert_eq!(facts["hasEarlier"], true);
    assert!(
        facts["windowFrom"].as_u64().expect("windowFrom") > 0,
        "the window has to say where it began: {facts}"
    );
    assert_eq!(facts["discarded"], 0, "the ring dropped nothing here");

    let beginning = tool
        .run(
            BackgroundParams {
                action: BackgroundAction::Output,
                task_id: Some(execution.id.as_str().to_owned()),
                cursor: Some(0),
                limit: Some(64),
                timeout: None,
                output_path: None,
            },
            context("ses_owner"),
        )
        .await
        .expect("the beginning");
    let facts = &beginning.metadata[zuno_tools::bg::BACKGROUND_METADATA_KEY];
    assert!(
        facts["output"]
            .as_str()
            .expect("window text")
            .contains("OPENING LINE"),
        "naming offset zero still reaches the head: {facts}"
    );
    assert_eq!(facts["windowFrom"], 0);
    assert_eq!(facts["hasEarlier"], false);
    assert_eq!(facts["hasMore"], true);
}

/// A read that names no size still gets a bounded window and a usable cursor.
#[tokio::test]
async fn an_output_read_returns_one_bounded_window_and_the_next_cursor() {
    let directory = tempfile::tempdir().expect("workspace");
    let service =
        Arc::new(BackgroundExecutionService::open(directory.path()).expect("background service"));
    let execution = service
        .start(input(directory.path(), "ses_owner", "printf 0123456789"))
        .expect("background command");
    service.wait(&execution.id, None).await.expect("settles");
    let tool = BackgroundTool::new(Arc::clone(&service));

    let first = tool
        .run(
            BackgroundParams {
                action: BackgroundAction::Output,
                task_id: Some(execution.id.as_str().to_owned()),
                cursor: Some(0),
                limit: Some(4),
                timeout: None,
                output_path: None,
            },
            context("ses_owner"),
        )
        .await
        .expect("first window");
    let facts = &first.metadata[zuno_tools::bg::BACKGROUND_METADATA_KEY];
    assert_eq!(facts["output"], "0123");
    assert_eq!(facts["cursor"], 4);
    assert_eq!(facts["hasMore"], true);
    assert_eq!(facts["fromDisk"], false);

    let second = tool
        .run(
            BackgroundParams {
                action: BackgroundAction::Output,
                task_id: Some(execution.id.as_str().to_owned()),
                cursor: Some(4),
                limit: Some(64),
                timeout: None,
                output_path: None,
            },
            context("ses_owner"),
        )
        .await
        .expect("second window");
    let facts = &second.metadata[zuno_tools::bg::BACKGROUND_METADATA_KEY];
    assert_eq!(facts["output"], "456789");
    assert_eq!(facts["cursor"], 10);
    assert_eq!(facts["hasMore"], false);
}

/// A window the ring has dropped comes back from the persisted file.
///
/// Before this, `bg` clamped the requested cursor forward to what the ring still held,
/// so a command whose output outgrew the 2 MiB buffer had its opening lines — the
/// failing assertion, the command line, the header — permanently unreachable through
/// the only tool that could read a background execution.
#[tokio::test]
async fn a_cursor_the_ring_dropped_is_served_from_the_persisted_file() {
    let directory = tempfile::tempdir().expect("workspace");
    let service =
        Arc::new(BackgroundExecutionService::open(directory.path()).expect("background service"));
    let total = zuno_pty::BUFFER_LIMIT + 4_096;
    let execution = service
        .start(input(
            directory.path(),
            "ses_owner",
            &format!("printf 'first line\\n'; head -c {total} /dev/zero | tr '\\0' x"),
        ))
        .expect("background command");
    service.wait(&execution.id, None).await.expect("settles");
    let tool = BackgroundTool::new(Arc::clone(&service));

    let recovered = tool
        .run(
            BackgroundParams {
                action: BackgroundAction::Output,
                task_id: Some(execution.id.as_str().to_owned()),
                cursor: Some(0),
                limit: Some(11),
                timeout: None,
                output_path: None,
            },
            context("ses_owner"),
        )
        .await
        .expect("dropped prefix");
    let facts = &recovered.metadata[zuno_tools::bg::BACKGROUND_METADATA_KEY];
    assert_eq!(facts["output"], "first line\n");
    assert_eq!(facts["fromDisk"], true);
    assert_eq!(facts["hasMore"], true);
    assert!(
        facts["discarded"].as_u64().expect("discarded") > 0,
        "{facts}"
    );
}

/// A caller that asks for everything gets a clamped window, not an unbounded transfer.
#[tokio::test]
async fn a_window_larger_than_the_ceiling_is_clamped_rather_than_refused() {
    let directory = tempfile::tempdir().expect("workspace");
    let store = tempfile::tempdir().expect("store");
    let store = ToolOutputStore::new(store.path());
    let stored = store
        .persist_bytes("shell", "ses_owner", &vec![b'x'; 200_000])
        .expect("persist");
    let service =
        Arc::new(BackgroundExecutionService::open(directory.path()).expect("background service"));
    let tool = BackgroundTool::new(service).with_output_store(store);

    let window = tool
        .run(
            BackgroundParams {
                action: BackgroundAction::Artifact,
                task_id: None,
                cursor: None,
                limit: Some(u64::MAX),
                timeout: None,
                output_path: Some(zuno_paths::wire_path(&stored.path)),
            },
            context("ses_owner"),
        )
        .await
        .expect("clamped window");
    let facts = &window.metadata[zuno_tools::bg::ARTIFACT_METADATA_KEY];
    assert_eq!(
        facts["windowBytes"],
        json!(zuno_tools::bg::MAX_WINDOW_BYTES)
    );
    assert_eq!(facts["totalBytes"], 200_000);
    assert_eq!(facts["hasMore"], true);
}

/// Withheld output is readable, in windows, by the session that produced it.
#[tokio::test]
async fn withheld_output_is_paged_back_by_the_session_that_produced_it() {
    let directory = tempfile::tempdir().expect("workspace");
    let store_dir = tempfile::tempdir().expect("store");
    let store = ToolOutputStore::new(store_dir.path());
    let stored = store
        .persist("shell", "ses_owner", "summary line\nsecond line\n")
        .expect("persist");
    let service =
        Arc::new(BackgroundExecutionService::open(directory.path()).expect("background service"));
    let tool = BackgroundTool::new(service).with_output_store(store);
    let path = zuno_paths::wire_path(&stored.path);

    let first = tool
        .run(
            BackgroundParams {
                action: BackgroundAction::Artifact,
                task_id: None,
                cursor: None,
                limit: Some(13),
                timeout: None,
                output_path: Some(path.clone()),
            },
            context("ses_owner"),
        )
        .await
        .expect("first window");
    assert!(
        first.output.starts_with("summary line\n"),
        "{}",
        first.output
    );
    let facts = &first.metadata[zuno_tools::bg::ARTIFACT_METADATA_KEY];
    assert_eq!(facts["cursor"], 13);
    assert_eq!(facts["hasMore"], true);
    assert_eq!(facts["totalBytes"], 25);
    // The model has to be told how to get the rest without being told to re-run the
    // command that produced it, which is never replayable.
    assert!(first.output.contains("`cursor: 13`"), "{}", first.output);

    let second = tool
        .run(
            BackgroundParams {
                action: BackgroundAction::Artifact,
                task_id: None,
                cursor: Some(13),
                limit: None,
                timeout: None,
                output_path: Some(path),
            },
            context("ses_owner"),
        )
        .await
        .expect("second window");
    assert_eq!(second.output, "second line\n");
    let facts = &second.metadata[zuno_tools::bg::ARTIFACT_METADATA_KEY];
    assert_eq!(facts["cursor"], 25);
    assert_eq!(facts["hasMore"], false);
}

/// One session cannot read another session's withheld output.
#[tokio::test]
async fn another_sessions_withheld_output_is_not_readable() {
    let directory = tempfile::tempdir().expect("workspace");
    let store_dir = tempfile::tempdir().expect("store");
    let store = ToolOutputStore::new(store_dir.path());
    let stored = store
        .persist("shell", "ses_owner", "secret")
        .expect("persist");
    let service =
        Arc::new(BackgroundExecutionService::open(directory.path()).expect("background service"));
    let tool = BackgroundTool::new(service).with_output_store(store);

    let error = tool
        .run(
            BackgroundParams {
                action: BackgroundAction::Artifact,
                task_id: None,
                cursor: None,
                limit: None,
                timeout: None,
                output_path: Some(zuno_paths::wire_path(&stored.path)),
            },
            context("ses_other"),
        )
        .await
        .expect_err("cross-session retrieval is rejected");
    assert!(
        zuno_error::source::describe(&error).contains("not written by this session"),
        "{error:?}"
    );
}

/// A parameter that means nothing for the action is refused by name.
#[tokio::test]
async fn a_parameter_the_action_cannot_use_is_refused_by_name() {
    let directory = tempfile::tempdir().expect("workspace");
    let service =
        Arc::new(BackgroundExecutionService::open(directory.path()).expect("background service"));
    let tool = BackgroundTool::new(service);

    for (action, params) in [
        (
            "cancel",
            BackgroundParams {
                action: BackgroundAction::Cancel,
                task_id: Some("bge_0".to_owned()),
                cursor: None,
                limit: Some(16),
                timeout: None,
                output_path: None,
            },
        ),
        (
            "output",
            BackgroundParams {
                action: BackgroundAction::Output,
                task_id: Some("bge_0".to_owned()),
                cursor: None,
                limit: None,
                timeout: None,
                output_path: Some("tool_ses_owner_1".to_owned()),
            },
        ),
        (
            "artifact",
            BackgroundParams {
                action: BackgroundAction::Artifact,
                task_id: Some("bge_0".to_owned()),
                cursor: None,
                limit: None,
                timeout: None,
                output_path: Some("tool_ses_owner_1".to_owned()),
            },
        ),
    ] {
        let error = tool
            .run(params, context("ses_owner"))
            .await
            .expect_err(action);
        assert!(
            zuno_error::source::describe(&error).contains("is not valid for this action"),
            "{action}: {error:?}"
        );
    }
}

/// The service root is enough to find the artifacts of the same checkout.
///
/// This is the production wiring: the composition root hands `bg` the shared execution
/// service and nothing else, so if the artifact directory were not derivable from that
/// service the retrieval path would exist only in tests.
#[tokio::test]
async fn a_service_rooted_in_a_checkout_reads_that_checkouts_withheld_output() {
    let worktree = tempfile::tempdir().expect("worktree");
    let background = zuno_paths::GeneratedDirectory::in_worktree(
        worktree.path(),
        &zuno_paths::generated::BACKGROUND_EXECUTIONS,
    );
    background.ensure().expect("background directory");
    let service =
        Arc::new(BackgroundExecutionService::open(background.path()).expect("background service"));
    let stored = ToolOutputStore::in_worktree(worktree.path())
        .persist("shell", "ses_owner", "the summary that was withheld")
        .expect("persist");

    let window = zuno_tools::BackgroundTool::new(service)
        .run(
            BackgroundParams {
                action: BackgroundAction::Artifact,
                task_id: None,
                cursor: None,
                limit: None,
                timeout: None,
                output_path: Some(zuno_paths::wire_path(&stored.path)),
            },
            context("ses_owner"),
        )
        .await
        .expect("derived store");
    assert_eq!(window.output, "the summary that was withheld");
}

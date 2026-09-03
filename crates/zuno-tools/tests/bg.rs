#![cfg(unix)]

mod support;

use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use zuno_pty::{
    BackgroundExecutionInput, BackgroundExecutionPurpose, BackgroundExecutionRetention,
    BackgroundExecutionService,
};
use zuno_tool::{
    AllowAll, NeverInterrupted, ToolContext, ToolEffect, ToolOutputStore, ToolReplayPolicy,
    TypedTool,
};
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
        purpose: BackgroundExecutionPurpose::Command,
        hard_ceiling: Duration::from_secs(5),
        retention: BackgroundExecutionRetention::Durable,
    }
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

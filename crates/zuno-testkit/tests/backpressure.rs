use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::Instant;

use tokio::sync::{broadcast, mpsc, watch};
use zuno_engine::r#loop::{TURN_EVENT_CHANNEL_CAPACITY, TurnEvent, event_channel};

const PROGRESS_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(target_os = "linux")]
const REAP_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(target_os = "linux")]
const HOST_SESSION_COUNT: usize = 2;
#[cfg(target_os = "linux")]
const GUARDED_HOST_KIND_COUNT: usize = 2;
#[cfg(target_os = "linux")]
const GUARDED_PROCESSES_PER_HOST: usize = 3;
#[cfg(target_os = "linux")]
const DIRECT_MCP_PROCESSES_PER_HOST: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Policy {
    LosslessBlock,
    BroadcastLag,
    LatestValue,
    CoalesceFull,
    RefuseNewest,
    SubscriberLag,
    ClosedDrop,
}

#[derive(Clone, Copy, Debug)]
struct ChannelGate {
    id: &'static str,
    file: &'static str,
    construction: &'static str,
    capacity: &'static str,
    policy: Policy,
    policy_file: &'static str,
    policy_needle: &'static str,
}

const CHANNELS: &[ChannelGate] = &[
    gate(
        "acp-outbound-frames",
        "zuno-acp/src/transport.rs",
        "let (output_tx, output_rx) = mpsc::channel(OUTBOUND_FRAME_CHANNEL_CAPACITY);",
        "OUTBOUND_FRAME_CHANNEL_CAPACITY=64",
        Policy::LosslessBlock,
        "zuno-acp/src/transport.rs",
        ".send(Outbound::Frame {",
    ),
    gate(
        "mcp-stdio-notifications",
        "zuno-mcp/src/stdio.rs",
        "let (notifications, _) = broadcast::channel(NOTIFICATION_CAPACITY);",
        "NOTIFICATION_CAPACITY=64",
        Policy::BroadcastLag,
        "zuno-mcp/src/protocol.rs",
        "notifications.send(notification)",
    ),
    gate(
        "mcp-stdio-tools-changed",
        "zuno-mcp/src/stdio.rs",
        "let (tools_changed, _) = broadcast::channel(TOOLS_CHANGED_CAPACITY);",
        "TOOLS_CHANGED_CAPACITY=16",
        Policy::BroadcastLag,
        "zuno-mcp/src/stdio.rs",
        "tools_changed.send(ToolsChanged { tools })",
    ),
    gate(
        "mcp-stdio-refresh",
        "zuno-mcp/src/stdio.rs",
        "let (refresh, refresh_receiver) = mpsc::channel(1);",
        "1",
        Policy::CoalesceFull,
        "zuno-mcp/src/protocol.rs",
        "refresh.try_send(())",
    ),
    gate(
        "mcp-remote-notifications",
        "zuno-mcp/src/remote/transport.rs",
        "let (notifications, _) = broadcast::channel(NOTIFICATION_CAPACITY);",
        "NOTIFICATION_CAPACITY=64",
        Policy::BroadcastLag,
        "zuno-mcp/src/protocol.rs",
        "notifications.send(notification)",
    ),
    gate(
        "mcp-remote-refresh",
        "zuno-mcp/src/remote/transport.rs",
        "let (refresh, _refresh_receiver) = mpsc::channel(1);",
        "1",
        Policy::ClosedDrop,
        "zuno-mcp/src/protocol.rs",
        "refresh.try_send(())",
    ),
    gate(
        "mcp-catalog-events",
        "zuno-mcp/src/catalog.rs",
        "let (events, _) = broadcast::channel(EVENT_CAPACITY);",
        "EVENT_CAPACITY=64",
        Policy::BroadcastLag,
        "zuno-mcp/src/catalog.rs",
        ".send(CatalogEvent::ToolsChanged { server })",
    ),
    gate(
        "mcp-lifecycle-events",
        "zuno-mcp/src/lifecycle.rs",
        "let (events, _) = broadcast::channel(EVENT_CAPACITY);",
        "EVENT_CAPACITY=64",
        Policy::BroadcastLag,
        "zuno-mcp/src/lifecycle.rs",
        "fn publish(&self, snapshot: McpServerSnapshot) { let _receivers = self.inner.events.send(McpServerEvent::StateChanged { snapshot }); }",
    ),
    gate(
        "mcp-lifecycle-cancel",
        "zuno-mcp/src/lifecycle.rs",
        "let (cancel, _receiver) = watch::channel(false);",
        "latest value",
        Policy::LatestValue,
        "zuno-mcp/src/lifecycle.rs",
        "if operation.kind == OperationKind::Connect && !enabled { let _replaced = operation.cancel.send(true);",
    ),
    gate(
        "lsp-manager-events",
        "zuno-lsp/src/manager.rs",
        "let (events, _) = broadcast::channel(64);",
        "64",
        Policy::BroadcastLag,
        "zuno-lsp/src/manager.rs",
        "manager.events.send(event)",
    ),
    gate(
        "lsp-server-changed",
        "zuno-lsp/src/manager.rs",
        "let (changed, _) = watch::channel(0_u64);",
        "latest value",
        Policy::LatestValue,
        "zuno-lsp/src/manager.rs",
        "server.changed.send(next)",
    ),
    gate(
        "lsp-supervisor-command",
        "zuno-lsp/src/manager.rs",
        "let (command, receiver) = mpsc::channel(4);",
        "4",
        Policy::LosslessBlock,
        "zuno-lsp/src/manager.rs",
        "server.command.send(SupervisorCommand::Shutdown).await",
    ),
    gate(
        "lsp-client-closed",
        "zuno-lsp/src/client.rs",
        "let (closed, _) = watch::channel(false);",
        "latest value",
        Policy::LatestValue,
        "zuno-lsp/src/client.rs",
        "inner.closed.send(true)",
    ),
    gate(
        "watch-events",
        "zuno-watch/src/lib.rs",
        "let (sender, receiver) = mpsc::channel(options.capacity);",
        "options.capacity",
        Policy::CoalesceFull,
        "zuno-watch/src/lib.rs",
        "guard.requeue(std::iter::once(event).chain(iterator), now)",
    ),
    gate(
        "engine-turn-events",
        "zuno-engine/src/loop.rs",
        "let (sender, receiver) = mpsc::channel(TURN_EVENT_CHANNEL_CAPACITY);",
        "TURN_EVENT_CHANNEL_CAPACITY=64",
        Policy::LosslessBlock,
        "zuno-engine/src/loop.rs",
        "self.sender.send(event)",
    ),
    gate(
        "turn-work-state-changes",
        "zuno-cli/src/cmd/child_turn.rs",
        "let (sender, _receiver) = watch::channel(0);",
        "latest value",
        Policy::LatestValue,
        "zuno-cli/src/cmd/child_turn.rs",
        "self.sender.send_modify(|generation| {",
    ),
    gate(
        "background-notification-target",
        "zuno-cli/src/cmd/background_notification.rs",
        "let (target_sender, target_receiver) = watch::channel(target);",
        "latest value",
        Policy::LatestValue,
        "zuno-cli/src/cmd/background_notification.rs",
        ".send_replace(target);",
    ),
    gate(
        "pty-subscriber-output",
        "zuno-pty/src/session.rs",
        "let (sender, output) = mpsc::channel(options.capacity.max(1));",
        "options.capacity.max(1)",
        Policy::SubscriberLag,
        "zuno-pty/src/session.rs",
        "subscriber.sender.try_send(PtyOutput::Chunk(chunk.to_vec()))",
    ),
    gate(
        "pty-lifecycle-events",
        "zuno-pty/src/lib.rs",
        "events: broadcast::channel(DEFAULT_EVENT_CAPACITY).0,",
        "DEFAULT_EVENT_CAPACITY=1024",
        Policy::BroadcastLag,
        "zuno-pty/src/lib.rs",
        "self.inner.events.send(event)",
    ),
    gate(
        "background-execution-events",
        "zuno-pty/src/background.rs",
        "let (events, _) = broadcast::channel(256);",
        "256",
        Policy::BroadcastLag,
        "zuno-pty/src/background.rs",
        "events.send(BackgroundExecutionEvent::Settled(info))",
    ),
    gate(
        "background-restored-info",
        "zuno-pty/src/background.rs",
        "let (info, _) = watch::channel(persisted.info);",
        "latest value",
        Policy::LatestValue,
        "zuno-pty/src/background.rs",
        "state.info.send_replace(info.clone())",
    ),
    gate(
        "background-restored-cancel",
        "zuno-pty/src/background.rs",
        "let (cancel, _) = watch::channel(false);",
        "latest value",
        Policy::LatestValue,
        "zuno-pty/src/background.rs",
        "state.cancel.send_replace(true)",
    ),
    gate(
        "background-live-info",
        "zuno-pty/src/background.rs",
        "let (info_sender, _) = watch::channel(info.clone());",
        "latest value",
        Policy::LatestValue,
        "zuno-pty/src/background.rs",
        "state.info.send_replace(info.clone())",
    ),
    gate(
        "background-live-cancel",
        "zuno-pty/src/background.rs",
        "let (cancel_sender, cancel_receiver) = watch::channel(false);",
        "latest value",
        Policy::LatestValue,
        "zuno-pty/src/background.rs",
        "state.cancel.send_replace(true)",
    ),
    gate(
        "background-output-chunks",
        "zuno-pty/src/background.rs",
        "let (chunks, receiver) = mpsc::channel::<Vec<u8>>(32);",
        "32",
        Policy::LosslessBlock,
        "zuno-pty/src/background.rs",
        "sender.send(buffer[..read].to_vec()).await",
    ),
    gate(
        "tui-terminal-events",
        "zuno-tui/src/app.rs",
        "mpsc::channel(TERMINAL_EVENT_CHANNEL_CAPACITY)",
        "TERMINAL_EVENT_CHANNEL_CAPACITY=64",
        Policy::LosslessBlock,
        "zuno-tui/src/app.rs",
        "sender.send(event).await",
    ),
    gate(
        "tui-prompts",
        "zuno-cli/src/cmd/tui.rs",
        "let (prompt_sender, prompt_receiver) = mpsc::channel(PROMPT_CHANNEL_CAPACITY);",
        "PROMPT_CHANNEL_CAPACITY=1",
        Policy::RefuseNewest,
        "zuno-tui/src/views/session.rs",
        "match prompts.try_send(TargetedPromptSubmission::root_with(PromptEnvelope::new(",
    ),
    gate(
        "tui-queue-mutations",
        "zuno-cli/src/cmd/tui.rs",
        "mpsc::channel(QUEUE_MUTATION_CHANNEL_CAPACITY);",
        "QUEUE_MUTATION_CHANNEL_CAPACITY=8",
        Policy::RefuseNewest,
        "zuno-tui/src/views/session.rs",
        "sink.try_send(mutation).is_ok()",
    ),
    gate(
        "tui-mcp-toggles",
        "zuno-cli/src/cmd/tui.rs",
        "let (mcp_toggle_sender, mcp_toggle_receiver) = mpsc::channel(MCP_TOGGLE_CHANNEL_CAPACITY);",
        "MCP_TOGGLE_CHANNEL_CAPACITY=1",
        Policy::RefuseNewest,
        "zuno-tui/src/views/session.rs",
        "if let Err(error) = requests.try_send(request) {",
    ),
    gate(
        "tui-picker-selections",
        "zuno-cli/src/cmd/tui.rs",
        "let (selection_sender, selection_receiver) = mpsc::channel(SELECTION_CHANNEL_CAPACITY);",
        "SELECTION_CHANNEL_CAPACITY=8",
        Policy::RefuseNewest,
        "zuno-tui/src/views/session.rs",
        "sink.try_send(selection).is_ok()",
    ),
    gate(
        "tui-turn-cancellations",
        "zuno-cli/src/cmd/tui.rs",
        "let (cancel_sender, cancel_receiver) = mpsc::channel(CANCEL_CHANNEL_CAPACITY);",
        "CANCEL_CHANNEL_CAPACITY=1",
        Policy::CoalesceFull,
        "zuno-tui/src/views/session.rs",
        "cancels.try_send(HardInterruptRequest::new(",
    ),
    gate(
        "tui-edit-signal",
        "zuno-cli/src/cmd/tui.rs",
        "let (edit_sender, edit_receiver) = mpsc::channel(EDIT_SIGNAL_CHANNEL_CAPACITY);",
        "EDIT_SIGNAL_CHANNEL_CAPACITY=1",
        Policy::CoalesceFull,
        "zuno-tui/src/views/lsp.rs",
        "let _nudged = self.wake.try_send(());",
    ),
    gate(
        // Lossless, not refuse-newest, and the change is the point: a language server's
        // finding is not a value the producer can regenerate, so a refused one left the
        // screen claiming a file was clean when nothing had checked it. The producer is a
        // task of its own whose only blocked work is the next query, so waiting for a slot
        // costs a query nobody could read yet.
        "tui-diagnostic-reports",
        "zuno-cli/src/cmd/tui.rs",
        "let (report_sender, report_receiver) = mpsc::channel(LSP_CHANNEL_CAPACITY);",
        "LSP_CHANNEL_CAPACITY=16",
        Policy::LosslessBlock,
        "zuno-cli/src/cmd/tui_lsp.rs",
        "if reports.send(report).await.is_err()",
    ),
    gate(
        "tui-questions",
        "zuno-cli/src/cmd/tui_question.rs",
        "let (waiting, pending) = mpsc::channel(QUESTION_CHANNEL_CAPACITY);",
        "QUESTION_CHANNEL_CAPACITY=8",
        Policy::LosslessBlock,
        "zuno-cli/src/cmd/tui_question.rs",
        ".send(PendingQuestion {",
    ),
    gate(
        "tui-prompt-history",
        "zuno-cli/src/cmd/tui.rs",
        "let (history_sender, history_receiver) = mpsc::channel(PROMPT_HISTORY_CHANNEL_CAPACITY);",
        "PROMPT_HISTORY_CHANNEL_CAPACITY=16",
        Policy::RefuseNewest,
        "zuno-tui/src/views/editor.rs",
        "let _recorded = sink.try_send(text.to_owned());",
    ),
    gate(
        "tui-editor-requests",
        "zuno-cli/src/cmd/tui.rs",
        "let (editor_sender, editor_receiver) = mpsc::channel(EDITOR_CHANNEL_CAPACITY);",
        "EDITOR_CHANNEL_CAPACITY=1",
        Policy::RefuseNewest,
        "zuno-tui/src/views/session.rs",
        "requests.try_send(request)",
    ),
    gate(
        "tui-editor-results",
        "zuno-cli/src/cmd/tui.rs",
        "let (editor_result_sender, editor_result_receiver) = mpsc::channel(EDITOR_CHANNEL_CAPACITY);",
        "EDITOR_CHANNEL_CAPACITY=1",
        Policy::LosslessBlock,
        "zuno-cli/src/cmd/tui.rs",
        "results.send(outcome).await",
    ),
    gate(
        "tui-worker-shutdown",
        "zuno-cli/src/cmd/tui.rs",
        "let (worker_shutdown, worker_shutdown_source) = watch::channel(false);",
        "latest value",
        Policy::LatestValue,
        "zuno-cli/src/cmd/tui.rs",
        "worker_shutdown.send(true)",
    ),
    gate(
        "tui-editor-shutdown",
        "zuno-cli/src/cmd/tui.rs",
        "let (editor_shutdown, editor_shutdown_source) = watch::channel(false);",
        "latest value",
        Policy::LatestValue,
        "zuno-cli/src/cmd/tui.rs",
        "editor_shutdown.send(true)",
    ),
];

const fn gate(
    id: &'static str,
    file: &'static str,
    construction: &'static str,
    capacity: &'static str,
    policy: Policy,
    policy_file: &'static str,
    policy_needle: &'static str,
) -> ChannelGate {
    ChannelGate {
        id,
        file,
        construction,
        capacity,
        policy,
        policy_file,
        policy_needle,
    }
}

#[test]
fn source_channel_inventory_matches_the_declared_registry() {
    let actual = source_channel_constructions();
    let expected: BTreeSet<String> = CHANNELS.iter().map(channel_source_key).collect();
    assert_eq!(
        actual, expected,
        "channel registry differs from production source"
    );
    assert_eq!(CHANNELS.len(), 39);

    let crates = crates_root();
    for entry in CHANNELS {
        assert!(
            !entry.capacity.is_empty(),
            "{} has no declared capacity",
            entry.id
        );
        let source = std::fs::read_to_string(crates.join(entry.policy_file))
            .unwrap_or_else(|error| panic!("read policy source for {}: {error}", entry.id));
        assert!(
            compact(&source).contains(&compact(entry.policy_needle)),
            "{} has no source evidence for {:?}: {}",
            entry.id,
            entry.policy,
            entry.policy_needle
        );
    }
}

macro_rules! channel_gate {
    ($name:ident, $id:literal) => {
        #[tokio::test]
        async fn $name() {
            run_channel_gate($id).await;
        }
    };
}

channel_gate!(
    acp_outbound_frames_applies_backpressure,
    "acp-outbound-frames"
);
channel_gate!(
    mcp_stdio_notifications_lag_one_subscriber,
    "mcp-stdio-notifications"
);
channel_gate!(
    mcp_stdio_tools_changed_lags_one_subscriber,
    "mcp-stdio-tools-changed"
);
channel_gate!(mcp_stdio_refresh_coalesces_full_signal, "mcp-stdio-refresh");
channel_gate!(
    mcp_remote_notifications_lag_one_subscriber,
    "mcp-remote-notifications"
);
channel_gate!(
    mcp_remote_refresh_drops_after_receiver_close,
    "mcp-remote-refresh"
);
channel_gate!(mcp_catalog_events_lag_one_subscriber, "mcp-catalog-events");
channel_gate!(
    mcp_lifecycle_events_lag_one_subscriber,
    "mcp-lifecycle-events"
);
channel_gate!(
    mcp_lifecycle_cancel_keeps_latest_value,
    "mcp-lifecycle-cancel"
);
channel_gate!(lsp_manager_events_lag_one_subscriber, "lsp-manager-events");
channel_gate!(lsp_server_changed_keeps_latest_value, "lsp-server-changed");
channel_gate!(
    lsp_supervisor_command_applies_backpressure,
    "lsp-supervisor-command"
);
channel_gate!(lsp_client_closed_keeps_latest_value, "lsp-client-closed");
channel_gate!(watch_events_coalesce_when_full, "watch-events");
channel_gate!(
    turn_work_state_changes_keep_latest_value,
    "turn-work-state-changes"
);
#[tokio::test]
async fn engine_turn_events_apply_backpressure() {
    let (sender, mut receiver) = event_channel();
    for index in 0..TURN_EVENT_CHANNEL_CAPACITY {
        sender
            .publish(turn_started(index))
            .await
            .expect("consumer remains open while filling the production channel");
    }

    let blocked_sender = sender.clone();
    let blocked = tokio::spawn(async move {
        blocked_sender
            .publish(turn_started(TURN_EVENT_CHANNEL_CAPACITY))
            .await
    });
    tokio::task::yield_now().await;
    assert!(
        !blocked.is_finished(),
        "TurnEventSender did not block when the production channel reached capacity"
    );
    observe_independent_progress().await;

    assert_eq!(receiver.recv().await, Some(turn_started(0)));
    tokio::time::timeout(PROGRESS_TIMEOUT, blocked)
        .await
        .expect("blocked TurnEventSender resumes after the consumer advances")
        .expect("producer task succeeds")
        .expect("production consumer remains open");
    assert_eq!(receiver.recv().await, Some(turn_started(1)));
}
channel_gate!(pty_subscriber_output_reports_lag, "pty-subscriber-output");
channel_gate!(
    pty_lifecycle_events_lag_one_subscriber,
    "pty-lifecycle-events"
);
channel_gate!(
    background_execution_events_lag_one_subscriber,
    "background-execution-events"
);
channel_gate!(
    background_restored_info_keeps_latest_value,
    "background-restored-info"
);
channel_gate!(
    background_restored_cancel_keeps_latest_value,
    "background-restored-cancel"
);
channel_gate!(
    background_live_info_keeps_latest_value,
    "background-live-info"
);
channel_gate!(
    background_live_cancel_keeps_latest_value,
    "background-live-cancel"
);
channel_gate!(
    background_output_chunks_apply_backpressure,
    "background-output-chunks"
);
channel_gate!(
    tui_terminal_events_apply_backpressure,
    "tui-terminal-events"
);
channel_gate!(tui_prompts_refuse_the_newest_prompt, "tui-prompts");
channel_gate!(
    tui_queue_mutations_refuse_the_newest_request,
    "tui-queue-mutations"
);
channel_gate!(tui_edit_signal_coalesces_when_full, "tui-edit-signal");
channel_gate!(
    tui_diagnostic_reports_apply_backpressure,
    "tui-diagnostic-reports"
);
channel_gate!(tui_mcp_toggles_refuse_the_newest_request, "tui-mcp-toggles");
channel_gate!(tui_questions_apply_backpressure, "tui-questions");
channel_gate!(tui_editor_requests_refuse_the_newest, "tui-editor-requests");
channel_gate!(tui_editor_results_apply_backpressure, "tui-editor-results");
channel_gate!(
    tui_worker_shutdown_keeps_latest_value,
    "tui-worker-shutdown"
);
channel_gate!(
    tui_editor_shutdown_keeps_latest_value,
    "tui-editor-shutdown"
);

#[cfg(target_os = "linux")]
#[test]
fn clean_shutdown_reaps_every_host_process_tree() {
    let directory = tempfile::tempdir().expect("G6 fixture directory");
    let (mut parent, ready, stop) = spawn_reaping_parent(directory.path(), true);
    wait_for_reaping_ready(&mut parent, &ready);
    let pids = snapshot_fixture_tree(&mut parent, true);

    std::fs::write(&stop, b"stop").expect("request clean host shutdown");
    let status = parent.wait().expect("wait for clean fixture parent");
    assert!(status.success(), "clean G6 fixture failed: {status}");
    assert_all_fixture_pids_exit(&pids);
}

#[cfg(target_os = "linux")]
#[test]
fn parent_sigkill_reaps_every_guarded_host_process_tree() {
    let directory = tempfile::tempdir().expect("G6 fixture directory");
    let (mut parent, ready, _stop) = spawn_reaping_parent(directory.path(), false);
    wait_for_reaping_ready(&mut parent, &ready);
    let pids = snapshot_fixture_tree(&mut parent, false);

    let pid = rustix::process::Pid::from_raw(parent.id() as i32).expect("non-zero parent PID");
    rustix::process::kill_process(pid, rustix::process::Signal::KILL)
        .expect("SIGKILL G6 fixture parent");
    let _status = parent.wait().expect("reap killed G6 fixture parent");
    assert_all_fixture_pids_exit(&pids);
}

#[cfg(target_os = "linux")]
#[test]
fn mcp_hosts_spawn_no_zuno_helper_processes() {
    let directory = tempfile::tempdir().expect("G6 fixture directory");
    let (mut parent, ready, stop) = spawn_reaping_parent(directory.path(), true);
    wait_for_reaping_ready(&mut parent, &ready);
    let pids = snapshot_fixture_tree(&mut parent, true);
    let commands = pids
        .iter()
        .map(|pid| (*pid, process_command_line(*pid)))
        .collect::<Vec<_>>();

    std::fs::write(&stop, b"stop").expect("request clean host shutdown");
    let status = parent.wait().expect("wait for clean fixture parent");
    assert!(status.success(), "clean G6 fixture failed: {status}");
    assert_all_fixture_pids_exit(&pids);

    let watchdogs = commands
        .iter()
        .filter(|(_, command)| command.contains("__zuno_child_guard watch-groups"))
        .collect::<Vec<_>>();
    assert!(
        watchdogs.is_empty(),
        "direct MCP must not start a Zuno watchdog: {commands:?}"
    );
    let mcp_supervisors = commands
        .iter()
        .filter(|(_, command)| {
            command.contains("__zuno_child_guard supervise")
                && command.split_whitespace().last() == Some("mcp")
        })
        .collect::<Vec<_>>();
    assert!(
        mcp_supervisors.is_empty(),
        "MCP commands should be direct children rather than per-server Zuno wrappers: \
         {mcp_supervisors:?}"
    );
}

async fn run_channel_gate(id: &str) {
    let entry = CHANNELS
        .iter()
        .find(|entry| entry.id == id)
        .unwrap_or_else(|| panic!("missing channel gate {id}"));
    match entry.policy {
        Policy::LosslessBlock => probe_lossless_block().await,
        Policy::BroadcastLag => probe_broadcast_lag().await,
        Policy::LatestValue => probe_latest_value().await,
        Policy::CoalesceFull | Policy::RefuseNewest | Policy::SubscriberLag => {
            probe_try_send_full().await;
        }
        Policy::ClosedDrop => probe_closed_drop().await,
    }
}

fn turn_started(index: usize) -> TurnEvent {
    TurnEvent::TurnStarted {
        session_id: format!("backpressure-{index}"),
    }
}

async fn probe_lossless_block() {
    let (sender, mut receiver) = mpsc::channel(1);
    sender.send(1_u8).await.expect("first item fits");
    let blocked = tokio::spawn(async move { sender.send(2_u8).await });
    tokio::task::yield_now().await;
    assert!(
        !blocked.is_finished(),
        "producer did not block on the bounded queue"
    );
    observe_independent_progress().await;
    assert_eq!(receiver.recv().await, Some(1));
    tokio::time::timeout(PROGRESS_TIMEOUT, blocked)
        .await
        .expect("blocked producer resumes")
        .expect("producer task")
        .expect("receiver remains open");
    assert_eq!(receiver.recv().await, Some(2));
}

async fn probe_broadcast_lag() {
    let (sender, mut stalled) = broadcast::channel(2);
    for value in 0_u8..4 {
        sender
            .send(value)
            .expect("stalled subscriber remains connected");
    }
    observe_independent_progress().await;
    assert!(matches!(
        stalled.recv().await,
        Err(broadcast::error::RecvError::Lagged(_))
    ));
    assert_eq!(stalled.recv().await, Ok(2));
}

async fn probe_latest_value() {
    let (sender, mut stalled) = watch::channel(0_u8);
    sender.send_replace(1);
    sender.send_replace(2);
    observe_independent_progress().await;
    stalled
        .changed()
        .await
        .expect("publisher remains connected");
    assert_eq!(*stalled.borrow_and_update(), 2);
}

async fn probe_try_send_full() {
    let (sender, mut stalled) = mpsc::channel(1);
    sender.try_send(1_u8).expect("first item fits");
    assert!(matches!(
        sender.try_send(2_u8),
        Err(mpsc::error::TrySendError::Full(2))
    ));
    observe_independent_progress().await;
    assert_eq!(stalled.recv().await, Some(1));
}

async fn probe_closed_drop() {
    let (sender, receiver) = mpsc::channel::<u8>(1);
    drop(receiver);
    assert!(matches!(
        sender.try_send(1),
        Err(mpsc::error::TrySendError::Closed(1))
    ));
    observe_independent_progress().await;
}

async fn observe_independent_progress() {
    let progress = Arc::new(AtomicUsize::new(0));
    let task_progress = Arc::clone(&progress);
    let task = tokio::spawn(async move {
        tokio::task::yield_now().await;
        task_progress.fetch_add(1, Ordering::Release);
    });
    tokio::time::timeout(PROGRESS_TIMEOUT, task)
        .await
        .expect("independent task makes progress")
        .expect("independent task succeeds");
    assert_eq!(
        progress.load(Ordering::Acquire),
        1,
        "no independent progress was observed"
    );
}

#[cfg(target_os = "linux")]
fn spawn_reaping_parent(directory: &Path, include_mcp: bool) -> (Child, PathBuf, PathBuf) {
    let ready = directory.join("ready");
    let stop = directory.join("stop");
    let workspace = directory.join("workspace");
    let child = Command::new(reaping_fixture_binary())
        .arg(if include_mcp {
            "parent"
        } else {
            "parent-guarded"
        })
        .arg(&ready)
        .arg(&stop)
        .arg(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn G6 fixture parent");
    (child, ready, stop)
}

#[cfg(target_os = "linux")]
fn reaping_fixture_binary() -> &'static Path {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| {
        let crates = crates_root();
        let root = crates.parent().expect("workspace root");
        let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args(["build", "-p", "zuno-reaping-fixture", "--offline"])
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status()
            .expect("build G6 fixture binary");
        assert!(status.success(), "building G6 fixture failed: {status}");
        let test_binary = std::env::current_exe().expect("current test executable");
        test_binary
            .parent()
            .and_then(Path::parent)
            .expect("target profile directory")
            .join(format!(
                "zuno-reaping-fixture{}",
                std::env::consts::EXE_SUFFIX
            ))
    })
}

#[cfg(target_os = "linux")]
fn wait_for_reaping_ready(parent: &mut Child, ready: &Path) {
    let started = Instant::now();
    loop {
        if ready.exists() {
            return;
        }
        if let Some(status) = parent.try_wait().expect("poll G6 fixture parent") {
            panic!("G6 fixture parent exited before ready: {status}");
        }
        assert!(
            started.elapsed() < REAP_TIMEOUT,
            "G6 fixture did not become ready"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Wait until every process the fixture will fork exists, then snapshot the tree.
///
/// `ready` proves each host answered its handshake, which is weaker than "the
/// fork storm finished": LSP and PTY use three-process chains (`supervise`
/// guard, host, grandchild), while MCP uses a direct host plus grandchild and no
/// Zuno helper. `PtyService::create` returns once
/// only its guard is spawned, so a PTY chain can still owe two PIDs. A loaded
/// runner can therefore observe a partial tree even after every host API has returned.
///
/// The threshold must stay at the full topology. Lowering it to whatever a slow
/// machine reaches would run the reaping assertion against a tree that was never
/// fully built, which is exactly what this gate exists to catch.
#[cfg(target_os = "linux")]
fn snapshot_fixture_tree(parent: &mut Child, include_mcp: bool) -> Vec<u32> {
    let root = parent.id();
    let direct_mcp = if include_mcp {
        DIRECT_MCP_PROCESSES_PER_HOST
    } else {
        0
    };
    let per_session = GUARDED_HOST_KIND_COUNT * GUARDED_PROCESSES_PER_HOST + direct_mcp;
    let minimum = 1 + HOST_SESSION_COUNT * per_session;
    let started = Instant::now();
    loop {
        let sample = zuno_testkit::perf::sample_process_tree(root, Instant::now())
            .expect("sample G6 fixture process tree");
        if sample.pids.len() >= minimum {
            return sample.pids;
        }
        if let Some(status) = parent.try_wait().expect("poll G6 fixture parent") {
            panic!("G6 fixture parent exited while its tree was still forking: {status}");
        }
        assert!(
            started.elapsed() < REAP_TIMEOUT,
            "G6 fixture tree never finished forking within {REAP_TIMEOUT:?}: \
             expected at least {minimum} PIDs, observed {} — {:?}. \
             This is an incomplete fork, not an incomplete reap: \
             no fixture process has been signalled yet.",
            sample.pids.len(),
            sample.pids
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
fn assert_all_fixture_pids_exit(pids: &[u32]) {
    let started = Instant::now();
    loop {
        let remaining = remaining_fixture_pids(pids);
        if remaining.is_empty() {
            return;
        }
        assert!(
            started.elapsed() < REAP_TIMEOUT,
            "G6 fixture tree finished forking with {} PIDs but {} survived {REAP_TIMEOUT:?} \
             after shutdown: {remaining:?}. \
             This is an incomplete reap, not an incomplete fork.",
            pids.len(),
            remaining.len()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
fn remaining_fixture_pids(pids: &[u32]) -> Vec<u32> {
    pids.iter()
        .copied()
        .filter(|pid| Path::new(&format!("/proc/{pid}")).exists())
        .collect()
}

#[cfg(target_os = "linux")]
fn process_command_line(pid: u32) -> String {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .unwrap_or_else(|error| panic!("read command line for process {pid}: {error}"))
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(String::from_utf8_lossy)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(target_os = "linux")]
#[test]
fn orphan_enumerator_reports_a_live_pid() {
    let pid = std::process::id();
    assert_eq!(remaining_fixture_pids(&[pid]), vec![pid]);
}

fn source_channel_constructions() -> BTreeSet<String> {
    let root = crates_root();
    let mut found = BTreeSet::new();
    for entry in walkdir::WalkDir::new(&root) {
        let entry = entry.expect("walk workspace sources");
        let path = entry.path();
        if !is_production_rust_source(path) {
            continue;
        }
        let relative = path.strip_prefix(&root).expect("source under crates root");
        let source = std::fs::read_to_string(path).expect("read Rust source");
        let production = production_lines(&source).join("\n");
        for construction in channel_constructions(&production) {
            found.insert(format!(
                "{}:{}",
                slash_path(relative),
                normalized(construction)
            ));
        }
    }
    found
}

fn channel_source_key(entry: &ChannelGate) -> String {
    format!("{}:{}", entry.file, normalized(entry.construction))
}

fn crates_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("zuno-testkit is inside crates")
        .to_path_buf()
}

fn is_production_rust_source(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "rs")
        && !path
            .components()
            .any(|component| component.as_os_str() == "tests")
        && !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with("_tests.rs"))
}

fn production_lines(source: &str) -> Vec<&str> {
    let lines: Vec<&str> = source.lines().collect();
    let mut result = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if lines[index].trim() == "#[cfg(test)]" {
            let mut next = index + 1;
            while next < lines.len() && lines[next].trim().starts_with("#[") {
                next += 1;
            }
            if next < lines.len() && lines[next].trim_start().starts_with("mod tests") {
                let mut depth = brace_delta(lines[next]);
                index = next + 1;
                while index < lines.len() && depth > 0 {
                    depth += brace_delta(lines[index]);
                    index += 1;
                }
                continue;
            }
        }
        result.push(lines[index]);
        index += 1;
    }
    result
}

fn brace_delta(line: &str) -> i32 {
    line.chars().fold(0, |depth, character| match character {
        '{' => depth + 1,
        '}' => depth - 1,
        _ => depth,
    })
}

fn channel_constructions(source: &str) -> Vec<&str> {
    const CONSTRUCTORS: &[&str] = &[
        "mpsc::channel",
        "mpsc::sync_channel",
        "mpsc::unbounded_channel",
        "broadcast::channel",
        "watch::channel",
        "async_channel::bounded",
        "async_channel::unbounded",
        "crossbeam_channel::bounded",
        "crossbeam_channel::unbounded",
        "flume::bounded",
        "flume::unbounded",
    ];
    let mut offsets = Vec::new();
    for constructor in CONSTRUCTORS {
        let mut search_from = 0;
        while let Some(relative) = source[search_from..].find(constructor) {
            let offset = search_from + relative;
            let suffix = &source[offset + constructor.len()..];
            let suffix = suffix.trim_start();
            if suffix.starts_with('(') || suffix.starts_with("::<") {
                offsets.push(offset);
            }
            search_from = offset + constructor.len();
        }
    }
    offsets.sort_unstable();
    offsets.dedup();
    offsets
        .into_iter()
        .map(|offset| {
            let start = source[..offset].rfind('\n').map_or(0, |index| index + 1);
            let call_open = call_open_parenthesis(source, offset);
            let call_close = matching_parenthesis(source, call_open);
            let mut end = call_close + 1;
            while source
                .as_bytes()
                .get(end)
                .is_some_and(u8::is_ascii_whitespace)
                && source.as_bytes()[end] != b'\n'
            {
                end += 1;
            }
            if source[end..].starts_with(".0") {
                end += 2;
            }
            while source
                .as_bytes()
                .get(end)
                .is_some_and(u8::is_ascii_whitespace)
                && source.as_bytes()[end] != b'\n'
            {
                end += 1;
            }
            if matches!(source.as_bytes().get(end), Some(b';' | b',')) {
                end += 1;
            }
            &source[start..end]
        })
        .collect()
}

fn call_open_parenthesis(source: &str, constructor_offset: usize) -> usize {
    let bytes = source.as_bytes();
    let mut angle_depth = 0_u32;
    for (relative, byte) in bytes[constructor_offset..].iter().enumerate() {
        match byte {
            b'<' => angle_depth += 1,
            b'>' => angle_depth = angle_depth.saturating_sub(1),
            b'(' if angle_depth == 0 => return constructor_offset + relative,
            _ => {}
        }
    }
    panic!("channel constructor has no call parenthesis")
}

fn matching_parenthesis(source: &str, open: usize) -> usize {
    let mut depth = 0_u32;
    for (relative, byte) in source.as_bytes()[open..].iter().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return open + relative;
                }
            }
            _ => {}
        }
    }
    panic!("channel constructor has no matching parenthesis")
}

fn normalized(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn compact(value: &str) -> String {
    value.split_whitespace().collect()
}

fn slash_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

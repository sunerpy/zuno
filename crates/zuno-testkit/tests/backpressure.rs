use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc, watch};
use zuno_engine::r#loop::{TURN_EVENT_CHANNEL_CAPACITY, TurnEvent, event_channel};

const PROGRESS_TIMEOUT: Duration = Duration::from_secs(1);
const REAP_TIMEOUT: Duration = Duration::from_secs(10);
const HOST_SESSION_COUNT: usize = 2;
const HOST_KIND_COUNT: usize = 4;
const CONTAINED_PROCESSES_PER_HOST: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Policy {
    LosslessBlock,
    BroadcastLag,
    LatestValue,
    CoalesceFull,
    RefuseNewest,
    SubscriberLag,
    ClosedDrop,
    SingleCompletion,
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
    exclusion: Option<&'static str>,
}

const CHANNELS: &[ChannelGate] = &[
    gate(
        "acp-outbound-frames",
        "zuno-acp/src/transport.rs",
        "let (output_tx, output_rx) = mpsc::channel(OUTBOUND_FRAME_CHANNEL_CAPACITY);",
        "OUTBOUND_FRAME_CHANNEL_CAPACITY=64",
        Policy::LosslessBlock,
        "zuno-acp/src/transport.rs",
        ".send(Outbound {",
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
        "prompts.try_send(text.clone())",
    ),
    excluded(
        "plugin-wasm-completion",
        "zuno-plugin/src/wasm.rs",
        "let (finished, receiver) = mpsc::channel();",
        "single completion result; no accumulating producer",
    ),
    excluded(
        "plugin-js-call-completion",
        "zuno-plugin/src/js/host.rs",
        "let (sender, receiver) = std::sync::mpsc::sync_channel(1);",
        "single completion result; producer sends exactly once",
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
        exclusion: None,
    }
}

const fn excluded(
    id: &'static str,
    file: &'static str,
    construction: &'static str,
    reason: &'static str,
) -> ChannelGate {
    ChannelGate {
        id,
        file,
        construction,
        capacity: "single completion",
        policy: Policy::SingleCompletion,
        policy_file: file,
        policy_needle: construction,
        exclusion: Some(reason),
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
    assert_eq!(
        CHANNELS
            .iter()
            .filter(|entry| entry.exclusion.is_none())
            .count(),
        17
    );
    assert_eq!(
        CHANNELS
            .iter()
            .filter(|entry| entry.exclusion.is_some())
            .count(),
        2
    );

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
        if entry.policy == Policy::SingleCompletion {
            assert!(
                entry.exclusion.is_some(),
                "{} needs an exclusion reason",
                entry.id
            );
        } else {
            assert!(
                entry.exclusion.is_none(),
                "{} cannot exclude a bounded queue",
                entry.id
            );
        }
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
channel_gate!(lsp_manager_events_lag_one_subscriber, "lsp-manager-events");
channel_gate!(lsp_server_changed_keeps_latest_value, "lsp-server-changed");
channel_gate!(
    lsp_supervisor_command_applies_backpressure,
    "lsp-supervisor-command"
);
channel_gate!(lsp_client_closed_keeps_latest_value, "lsp-client-closed");
channel_gate!(watch_events_coalesce_when_full, "watch-events");
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
    tui_terminal_events_apply_backpressure,
    "tui-terminal-events"
);
channel_gate!(tui_prompts_refuse_the_newest_prompt, "tui-prompts");

#[cfg(target_os = "linux")]
#[test]
fn clean_shutdown_reaps_every_host_process_tree() {
    let directory = tempfile::tempdir().expect("G6 fixture directory");
    let (mut parent, ready, stop) = spawn_reaping_parent(directory.path());
    wait_for_reaping_ready(&mut parent, &ready);
    let pids = snapshot_fixture_tree(parent.id());

    std::fs::write(&stop, b"stop").expect("request clean host shutdown");
    let status = parent.wait().expect("wait for clean fixture parent");
    assert!(status.success(), "clean G6 fixture failed: {status}");
    assert_all_fixture_pids_exit(&pids);
}

#[cfg(target_os = "linux")]
#[test]
fn parent_sigkill_reaps_every_host_process_tree() {
    let directory = tempfile::tempdir().expect("G6 fixture directory");
    let (mut parent, ready, _stop) = spawn_reaping_parent(directory.path());
    wait_for_reaping_ready(&mut parent, &ready);
    let pids = snapshot_fixture_tree(parent.id());

    let pid = rustix::process::Pid::from_raw(parent.id() as i32).expect("non-zero parent PID");
    rustix::process::kill_process(pid, rustix::process::Signal::KILL)
        .expect("SIGKILL G6 fixture parent");
    let _status = parent.wait().expect("reap killed G6 fixture parent");
    assert_all_fixture_pids_exit(&pids);
}

async fn run_channel_gate(id: &str) {
    let entry = CHANNELS
        .iter()
        .find(|entry| entry.id == id)
        .unwrap_or_else(|| panic!("missing channel gate {id}"));
    assert!(
        entry.exclusion.is_none(),
        "excluded channel cannot run a gate: {id}"
    );
    match entry.policy {
        Policy::LosslessBlock => probe_lossless_block().await,
        Policy::BroadcastLag => probe_broadcast_lag().await,
        Policy::LatestValue => probe_latest_value().await,
        Policy::CoalesceFull | Policy::RefuseNewest | Policy::SubscriberLag => {
            probe_try_send_full().await;
        }
        Policy::ClosedDrop => probe_closed_drop().await,
        Policy::SingleCompletion => panic!("single-completion channel cannot accumulate: {id}"),
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
fn spawn_reaping_parent(directory: &Path) -> (Child, PathBuf, PathBuf) {
    let ready = directory.join("ready");
    let stop = directory.join("stop");
    let workspace = directory.join("workspace");
    let child = Command::new(reaping_fixture_binary())
        .arg("parent")
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

#[cfg(target_os = "linux")]
fn snapshot_fixture_tree(root: u32) -> Vec<u32> {
    let sample = zuno_testkit::perf::sample_process_tree(root, Instant::now())
        .expect("sample complete G6 fixture process tree");
    let minimum = 1 + HOST_SESSION_COUNT * HOST_KIND_COUNT * CONTAINED_PROCESSES_PER_HOST;
    assert!(
        sample.pids.len() >= minimum,
        "G6 fixture tree is incomplete: expected at least {minimum}, got {:?}",
        sample.pids
    );
    sample.pids
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
            "owned G6 fixture PIDs were not reaped: {remaining:?}"
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

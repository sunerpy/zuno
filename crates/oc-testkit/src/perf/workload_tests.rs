use std::path::Path;

use std::time::{Duration, Instant};

use super::baseline::{RssSample, WorkloadName};
use super::fixtures::{provider_config, write_memory_driver_tool};
use super::workload::{
    TurnPlan, completed_tool_turns, hydration_is_settled, oracle_command, peak_after_warm_up,
};

#[test]
fn oracle_command_keeps_auto_approval_enabled_for_tool_turns() {
    // Given: the unattended TypeScript oracle and one cassette-backed tool turn.
    let oracle = Path::new("/opt/opencode");

    // When: the PTY command is assembled.
    let command = oracle_command(oracle, None);
    let arguments: Vec<&str> = command.split_whitespace().collect();

    // Then: auto approval reaches the full TUI instead of mini mode discarding it.
    assert!(arguments.contains(&"--auto"));
    assert!(!arguments.contains(&"--mini"), "{command}");
}

#[test]
fn baseline_config_allows_custom_tool_without_ui_permission_flow() {
    // Given: an unattended baseline run using the custom get_weather tool.
    let config: serde_json::Value =
        serde_json::from_str(&provider_config("http://127.0.0.1:1234")).expect("valid config");

    // When: the wildcard permission is read from the generated config.
    let action = &config["permission"]["*"];

    // Then: the tool is allowed without an interactive permission round trip.
    assert_eq!(action, "allow");
}

#[test]
fn baseline_tool_fixture_provisions_offline_dependency_state() {
    // Given: a closed temporary project that will load one custom tool.
    let env = crate::ScriptedEnv::new().expect("scripted environment");

    // When: the baseline tool fixture is installed.
    write_memory_driver_tool(&env, false).expect("write baseline tool fixture");

    // Then: opencode sees completed local npm state and never enters network reification.
    assert!(env.project().join(".opencode/node_modules").is_dir());
    assert!(env.xdg_config().join("opencode/node_modules").is_dir());
    let lock = std::fs::read_to_string(env.project().join(".opencode/package-lock.json"))
        .expect("read fixture lockfile");
    let lock: serde_json::Value = serde_json::from_str(&lock).expect("valid fixture lockfile");
    assert_eq!(
        lock["packages"][""]["dependencies"]["@opencode-ai/plugin"],
        "*"
    );
}

#[test]
fn baseline_tool_fixture_has_no_external_import() {
    // Given: custom tools loaded from an isolated project without packages.
    let fixtures = [
        include_str!("single_turn_tool.ts.txt"),
        include_str!("soak_tool.ts.txt"),
    ];

    // When/Then: each module is self-contained at the machine-consumed import boundary.
    for fixture in fixtures {
        assert!(!fixture.contains("@opencode-ai/plugin"), "{fixture}");
    }
}

#[test]
fn the_text_prelude_does_not_count_as_a_completed_tool_turn() {
    // Given: the tool-free prelude request every run opens with, followed by the
    // first of the two requests one cassette-backed tool loop makes.
    // When: completed tool turns are derived from provider traffic.
    // Then: the prelude is excluded and the unfinished tool loop stays incomplete,
    // while the loop's second request completes exactly one turn.
    assert_eq!(completed_tool_turns(2), 0);
    assert_eq!(completed_tool_turns(3), 1);
}

#[test]
fn only_a_restored_session_needs_its_first_turn_typed() {
    // Given: a new session, which submits `--prompt` itself, and a restored one,
    // which discards it in favour of the session's saved draft input.
    // When: each plan is asked whether the harness must type the first turn.
    // Then: only the restored session's turn is submitted through the PTY.
    assert!(!TurnPlan::for_session(false).submit_first_turn);
    assert!(TurnPlan::for_session(true).submit_first_turn);
}

#[test]
fn a_restored_sessions_first_turn_waits_for_the_hydration_gate() {
    // Given: a run that has just started and one past the 90s hydration gate.
    let cold = Instant::now();
    let warm = cold - Duration::from_secs(91);

    // When: the first turn of a restored session asks whether it may be typed.
    // Then: only the hydrated run submits, while later turns are never delayed.
    assert!(!hydration_is_settled(0, cold));
    assert!(hydration_is_settled(0, warm));
    assert!(hydration_is_settled(1, cold));
}

#[test]
fn a_bounded_workloads_peak_includes_its_cold_start() {
    // Given: a 100-second trace whose largest sample is the cold-start spike, as
    // W-idle's real traces are.
    let samples = [
        (2_000_u64, 900_000_u64),
        (30_000, 950_000),
        (95_000, 700_000),
    ]
    .map(|(elapsed_ms, total_rss_kib)| RssSample {
        elapsed_ms,
        total_rss_kib,
        pids: vec![1],
    });

    // When: each workload takes its peak over that trace.
    let idle = peak_after_warm_up(&samples, WorkloadName::WIdle);
    let real = peak_after_warm_up(&samples, WorkloadName::WReal);
    let soak = peak_after_warm_up(&samples, WorkloadName::WSoak);

    // Then: the bounded workloads keep the spike they exist to measure, and only
    // the soak drops it as the startup transient it is for a multi-hour run.
    assert_eq!(idle, Some(950_000));
    assert_eq!(real, Some(950_000));
    assert_eq!(soak, Some(700_000));
}

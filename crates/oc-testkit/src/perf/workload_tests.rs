use std::path::Path;

use super::fixtures::{provider_config, write_memory_driver_tool};
use super::workload::{completed_tool_turns, oracle_command};

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
fn title_request_does_not_count_as_a_completed_tool_turn() {
    // Given: one title request followed by the first request of a tool loop.
    let captured_requests = 2;

    // When: completed tool turns are derived from provider traffic.
    let turns = completed_tool_turns(captured_requests);

    // Then: the title prelude is excluded and the unfinished tool loop stays incomplete.
    assert_eq!(turns, 0);
}

use std::process::Command;

use clap::CommandFactory as _;
use oc_cli::{
    BUILD_ID, COMPATIBILITY_VERSION, Cli, Disposition, dispositions, long_version, user_agent,
    validate_upstream_surface,
};

const UPSTREAM_COMMANDS: &str = include_str!("fixtures/upstream-commands-1.18.13.txt");

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_opencode-rust"))
}

#[test]
fn surface_every_upstream_command_has_exactly_one_disposition() {
    let upstream: Vec<&str> = UPSTREAM_COMMANDS
        .lines()
        .filter(|line| !line.is_empty())
        .collect();
    validate_upstream_surface(UPSTREAM_COMMANDS)
        .expect("the frozen upstream surface must be covered");
    assert_eq!(
        upstream.len(),
        23,
        "the mechanically extracted fixture changed; inspect the one-to-one validation above"
    );

    for symbol in upstream {
        let count = dispositions()
            .iter()
            .filter(|entry| entry.upstream_symbol == symbol)
            .count();
        assert_eq!(count, 1, "{symbol} must have exactly one disposition");
    }
}

#[test]
fn surface_failure_scenario_detects_an_unhandled_fixture_command() {
    let mutated = format!("{UPSTREAM_COMMANDS}FutureCommand\n");
    let error = validate_upstream_surface(&mutated)
        .expect_err("an upstream command without a disposition must fail closed");
    assert!(error.to_string().contains("FutureCommand"));
}

#[test]
fn surface_registered_commands_match_their_dispositions() {
    let command = Cli::command();
    let registered: Vec<&str> = command
        .get_subcommands()
        .map(clap::Command::get_name)
        .collect();

    for entry in dispositions() {
        match entry.disposition {
            Disposition::Implemented | Disposition::Rejected => assert!(
                registered.contains(&entry.command),
                "{} is {:?} and must be registered",
                entry.upstream_symbol,
                entry.disposition
            ),
            Disposition::NotRegistered => assert!(
                !registered.contains(&entry.command),
                "{} is explicitly deferred and must not be registered early",
                entry.upstream_symbol
            ),
        }
    }

    assert!(registered.contains(&"completion"));
    assert!(registered.contains(&"providers"));
}

#[test]
fn surface_compatibility_version_and_rust_identity_are_separate() {
    assert_eq!(COMPATIBILITY_VERSION, "1.18.13");
    assert_ne!(BUILD_ID, COMPATIBILITY_VERSION);
    assert!(long_version().contains(BUILD_ID));
    assert!(long_version().contains(COMPATIBILITY_VERSION));
    assert!(user_agent().starts_with("opencode-rust/"));
    assert!(!user_agent().starts_with("opencode/"));

    let short = binary().arg("--version").output().expect("run --version");
    assert!(short.status.success());
    assert_eq!(
        String::from_utf8_lossy(&short.stdout).trim(),
        COMPATIBILITY_VERSION
    );

    let long = binary()
        .args(["--version", "--long"])
        .output()
        .expect("run --version --long");
    assert!(long.status.success());
    let stdout = String::from_utf8_lossy(&long.stdout);
    assert!(stdout.contains(BUILD_ID));
    assert!(stdout.contains(COMPATIBILITY_VERSION));
}

#[test]
fn surface_console_is_rejected_with_scope_and_replacement() {
    let output = binary().arg("console").output().expect("run console");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("hosted"), "{stderr}");
    assert!(stderr.contains("excluded"), "{stderr}");
    assert!(stderr.contains("providers"), "{stderr}");
}

#[test]
fn surface_providers_keeps_the_auth_alias() {
    let command = Cli::command();
    let providers = command
        .get_subcommands()
        .find(|subcommand| subcommand.get_name() == "providers")
        .expect("providers command");
    assert!(providers.get_all_aliases().any(|alias| alias == "auth"));
}

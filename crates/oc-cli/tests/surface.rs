use std::process::Command;

use clap::{CommandFactory as _, Parser as _};
use oc_cli::{
    Action, BUILD_ID, COMPATIBILITY_VERSION, Cli, CommandDispatcher as _, Disposition,
    dispositions, long_version, user_agent, validate_upstream_surface,
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

// ---------------------------------------------------------------------------
// The invariant todo 116 exists to make structural
// ---------------------------------------------------------------------------

/// One invocation per command, used to reach its dispatch arm without running it.
///
/// Every [`Disposition::Implemented`] row must appear here, and
/// [`surface_every_implemented_command_actually_has_a_handler`] asserts that, so
/// registering a command cannot escape the handler check by simply not being
/// probed.
const IMPLEMENTED_PROBES: &[(&str, &[&str])] = &[
    ("agent", &["agent", "list"]),
    ("db", &["db"]),
    ("debug", &["debug", "paths"]),
    ("export", &["export", "ses_probe000000000000000000000a"]),
    ("import", &["import", "probe.json"]),
    ("mcp", &["mcp", "list"]),
    ("models", &["models"]),
    ("providers", &["providers", "list"]),
    ("run", &["run"]),
    ("serve", &["serve"]),
    ("session", &["session", "list"]),
    ("tui", &["tui"]),
];

/// Resolve one argv to the dispatch request the CLI would hand a handler.
fn dispatch_request(argv: &[&str]) -> Box<oc_cli::DispatchRequest> {
    let cli = Cli::try_parse_from(std::iter::once("opencode-rust").chain(argv.iter().copied()))
        .unwrap_or_else(|error| panic!("{argv:?} must parse: {error}"));
    match cli.action(&oc_paths::Env::empty()) {
        Action::Dispatch(request) => request,
        other => panic!("{argv:?} must dispatch, got {other:?}"),
    }
}

/// **No command advertised as implemented may route to the pending handler.**
///
/// This is the check the three mechanisms that agreed `export` worked could not
/// perform. The disposition table, the generated matrix and the plan checkbox all
/// described *intent*; this reads the dispatch arm the parsed command actually
/// lands on, so a row claiming `implemented` next to a stub fails here.
///
/// Anchoring on [`oc_cli::DispatchArguments::is_pending`] rather than on the
/// table is the whole point: comparing the disposition table to the generated
/// matrix would only prove that two documents derived from the same table agree
/// with each other, which they did throughout the defect.
#[test]
fn surface_no_implemented_disposition_routes_to_the_pending_handler() {
    let mut liars = Vec::new();
    for entry in dispositions() {
        if entry.disposition != Disposition::Implemented {
            continue;
        }
        let Some((_, argv)) = IMPLEMENTED_PROBES
            .iter()
            .find(|(name, _)| *name == entry.command)
        else {
            continue;
        };
        if dispatch_request(argv).args.is_pending() {
            liars.push(format!(
                "`{}` ({}) is recorded as implemented but routes to the pending handler",
                entry.command, entry.upstream_symbol
            ));
        }
        if oc_cli::pending_reason(entry.command).is_some() {
            liars.push(format!(
                "`{}` is recorded as implemented and also listed in PENDING_COMMANDS",
                entry.command
            ));
        }
    }
    assert!(liars.is_empty(), "{}", liars.join("\n"));
}

/// Every implemented command is probed, so the check above cannot be narrowed by
/// omission.
#[test]
fn surface_every_implemented_command_actually_has_a_handler() {
    let implemented: Vec<&str> = dispositions()
        .iter()
        .filter(|entry| entry.disposition == Disposition::Implemented)
        .map(|entry| entry.command)
        .collect();
    for command in &implemented {
        assert!(
            IMPLEMENTED_PROBES.iter().any(|(name, _)| name == command),
            "`{command}` is implemented but has no probe in IMPLEMENTED_PROBES"
        );
    }
    for (command, _) in IMPLEMENTED_PROBES {
        assert!(
            implemented.contains(command),
            "IMPLEMENTED_PROBES names `{command}`, which is not an implemented disposition"
        );
    }
    assert_eq!(implemented.len(), IMPLEMENTED_PROBES.len());
}

/// The pending set is exactly what it claims, in both directions.
#[test]
fn surface_the_pending_command_roster_is_complete_and_accurate() {
    for (command, reason) in oc_cli::PENDING_COMMANDS {
        assert!(
            !reason.is_empty(),
            "`{command}` is pending without a recorded reason"
        );
        assert!(
            dispatch_request(&[command]).args.is_pending(),
            "`{command}` is listed as pending but reaches a handler"
        );
        assert!(
            oc_cli::disposition_for(command)
                .is_none_or(|entry| entry.disposition != Disposition::Implemented),
            "`{command}` is pending and must not be recorded as implemented"
        );
    }

    // Closed in the other direction: nothing else in the registered tree is a
    // stub. Without this, a newly stubbed command would simply be absent from the
    // roster and no test would notice.
    let registered: Vec<String> = Cli::command()
        .get_subcommands()
        .map(|subcommand| subcommand.get_name().to_owned())
        .collect();
    let probes: Vec<&str> = IMPLEMENTED_PROBES
        .iter()
        .map(|(_, argv)| argv[0])
        .chain(oc_cli::PENDING_COMMANDS.iter().map(|(name, _)| *name))
        .collect();
    for name in &registered {
        if oc_cli::disposition_for(name)
            .is_some_and(|entry| entry.disposition == Disposition::Rejected)
        {
            continue;
        }
        assert!(
            probes.contains(&name.as_str()),
            "`{name}` is registered but is neither probed as implemented, recorded as \
             rejected, nor listed in PENDING_COMMANDS"
        );
    }
}

/// The negative control: the detector above can actually see a stub.
///
/// A test asserting "nothing is pending" passes trivially once nothing is, and
/// would keep passing if the pending machinery were removed and a future stub
/// failed some other way. This drives a request through the real
/// [`oc_cli::PendingCommandDispatcher`] and asserts it produces the failure the
/// other tests look for, so the detector is proven live rather than assumed.
#[test]
fn surface_failure_scenario_the_pending_handler_is_detectable() {
    let request = dispatch_request(&["completion"]);
    assert!(request.args.is_pending());

    let mut dispatcher = oc_cli::PendingCommandDispatcher;
    let error = dispatcher
        .dispatch(*request)
        .expect_err("the pending handler must fail");
    assert_eq!(error.command, "completion");
    let rendered = error.to_string();
    assert!(rendered.contains("not available"), "{rendered}");
    assert!(
        !rendered.contains("todo 56"),
        "a user-facing failure must not cite a closed build task: {rendered}"
    );
}

/// `export` in particular no longer reports a missing handler.
///
/// Named after the defect rather than after a mechanism, so the reproduction from
/// `.omo/evidence/F3-REPORT.md` section 9 stays executable: this is the exact
/// invocation that printed "`export` is registered, but its handler is pending
/// todo 56" and exited 1.
#[test]
fn surface_export_no_longer_reports_a_pending_handler() {
    let output = binary()
        .args(["export", "ses_738026eec17c4c33ba2fe3bfc90d8b01"])
        .env(
            "OPENCODE_DB",
            std::env::temp_dir().join("oc-surface-export.db"),
        )
        .output()
        .expect("run export");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("handler is pending"),
        "export still reports a missing handler: {stderr}"
    );
    assert!(
        !dispatch_request(&["export", "ses_738026eec17c4c33ba2fe3bfc90d8b01"])
            .args
            .is_pending()
    );
}

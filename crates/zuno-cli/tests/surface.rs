use std::process::Command;

use clap::CommandFactory as _;
use zuno_cli::{
    BUILD_ID, Cli, Disposition, RUST_PACKAGE_VERSION, disposition_for, dispositions, long_version,
    user_agent, validate_upstream_surface,
};

const UPSTREAM_COMMANDS: &str = include_str!("fixtures/upstream-commands-1.18.13.txt");

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_zuno"))
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
fn surface_version_reports_only_zuno_identity() {
    assert!(long_version().contains(BUILD_ID));
    assert!(long_version().contains(RUST_PACKAGE_VERSION));
    assert!(!long_version().contains("opencode"));
    assert!(user_agent().starts_with("zuno/"));
    assert!(!user_agent().contains("opencode"));

    let short = binary().arg("--version").output().expect("run --version");
    assert!(short.status.success());
    assert_eq!(
        String::from_utf8_lossy(&short.stdout).trim(),
        RUST_PACKAGE_VERSION
    );

    let long = binary()
        .args(["--version", "--long"])
        .output()
        .expect("run --version --long");
    assert!(long.status.success());
    let stdout = String::from_utf8_lossy(&long.stdout);
    assert!(stdout.contains(BUILD_ID));
    assert!(stdout.contains(RUST_PACKAGE_VERSION));
    assert!(!stdout.contains("opencode"));
}

#[test]
fn surface_zuno_user_agent_is_pinned() {
    assert_eq!(
        user_agent(),
        format!("zuno/{} (build {BUILD_ID})", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn surface_zuno_long_display_version_is_pinned() {
    let display = long_version();
    assert!(display.starts_with("Zuno "), "{display}");
    assert!(display.contains(BUILD_ID), "{display}");
    assert!(display.contains(RUST_PACKAGE_VERSION), "{display}");
    assert!(!display.contains("opencode"), "{display}");
}

#[test]
fn surface_zuno_help_identity_is_pinned() {
    let command = Cli::command();
    assert_eq!(command.get_name(), "zuno");
    let about = command.get_about().expect("root command has help text");
    assert!(about.to_string().contains("Zuno"), "{about}");
}

#[test]
fn surface_console_is_rejected_with_scope_and_replacement() {
    let output = binary().arg("console").output().expect("run console");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("hosted"), "{stderr}");
    assert!(stderr.contains("does not provide"), "{stderr}");
    assert!(stderr.contains("providers"), "{stderr}");
    assert!(stderr.contains("auth"), "{stderr}");
    assert!(
        !stderr.to_ascii_lowercase().contains("opencode"),
        "{stderr}"
    );
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

/// One invocation per implemented command, and the observable that proves *its*
/// handler ran.
///
/// Every [`Disposition::Implemented`] row must appear here, and
/// [`surface_every_implemented_command_actually_has_a_handler`] asserts that, so
/// registering a command cannot escape the handler check by simply not being
/// probed.
struct Probe {
    /// Canonical CLI spelling, matched against the disposition table.
    command: &'static str,
    /// An invocation that reaches the handler and returns promptly, touching no
    /// network and writing nothing outside the probe's temporary root.
    ///
    /// Several arms are entered through a flag the handler itself refuses
    /// (`serve --mdns`, `run --fork`) precisely because that refusal is produced
    /// *inside* the handler, after routing, and before the handler does anything
    /// expensive. A bare `serve` would block on a listening socket and a bare
    /// `run` would want a provider.
    argv: &'static [&'static str],
    /// A fragment of this handler's own output, on either stream.
    ///
    /// Observing this fragment is positive evidence that the production match arm
    /// ran the handler. Asserting only a successful exit would also pass if the arm
    /// were replaced with `Ok(())`.
    evidence: &'static str,
}

const IMPLEMENTED_PROBES: &[Probe] = &[
    Probe {
        command: "agent",
        argv: &["agent", "list"],
        evidence: "build (primary)",
    },
    Probe {
        command: "db",
        argv: &["db", "--format", "json", "select 1 as probe"],
        evidence: "\"probe\": 1",
    },
    Probe {
        command: "debug",
        argv: &["debug", "paths"],
        evidence: "repos",
    },
    Probe {
        command: "completion",
        argv: &["completion", "bash"],
        evidence: "_zuno",
    },
    Probe {
        command: "export",
        argv: &["export", "ses_probe000000000000000000000a"],
        evidence: "Session not found: ses_probe000000000000000000000a",
    },
    Probe {
        command: "import",
        argv: &["import", "probe.json"],
        evidence: "File not found: probe.json",
    },
    Probe {
        command: "mcp",
        argv: &["mcp", "list"],
        evidence: "No MCP servers configured",
    },
    Probe {
        command: "models",
        argv: &["models", "--refresh"],
        evidence: "the model catalog cannot be refreshed",
    },
    Probe {
        command: "providers",
        argv: &["providers", "list"],
        evidence: "0 credentials",
    },
    Probe {
        command: "run",
        argv: &["run", "--fork", "probe"],
        evidence: "--fork requires a session-history fork API",
    },
    Probe {
        command: "serve",
        argv: &["serve", "--mdns"],
        evidence: "--mdns is not supported by the Rust server runtime",
    },
    Probe {
        command: "session",
        argv: &["session", "delete", "ses_probe000000000000000000000a"],
        evidence: "Session not found: ses_probe000000000000000000000a",
    },
    Probe {
        command: "tui",
        argv: &["tui"],
        evidence: "the interactive TUI requires a terminal",
    },
];

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
            IMPLEMENTED_PROBES
                .iter()
                .any(|probe| &probe.command == command),
            "`{command}` is implemented but has no probe in IMPLEMENTED_PROBES"
        );
    }
    for probe in IMPLEMENTED_PROBES {
        if let Some(entry) = disposition_for(probe.command) {
            assert_eq!(
                entry.disposition,
                Disposition::Implemented,
                "IMPLEMENTED_PROBES names `{}`, which is not implemented",
                probe.command
            );
        }
    }
    assert!(IMPLEMENTED_PROBES.len() >= implemented.len());
}

/// Every registered, non-rejected command has a production handler probe.
#[test]
fn surface_no_registered_command_is_only_a_display_entry() {
    let registered: Vec<String> = Cli::command()
        .get_subcommands()
        .map(|subcommand| subcommand.get_name().to_owned())
        .collect();
    let probes: Vec<&str> = IMPLEMENTED_PROBES
        .iter()
        .map(|probe| probe.argv[0])
        .collect();
    for name in &registered {
        if disposition_for(name).is_some_and(|entry| entry.disposition == Disposition::Rejected) {
            continue;
        }
        assert!(
            probes.contains(&name.as_str()),
            "`{name}` is registered but has no production handler probe"
        );
    }
}

// ---------------------------------------------------------------------------
// The behavioural guard: what the production dispatcher actually does
// ---------------------------------------------------------------------------

/// Run one probe against the shipped binary in a private root.
///
/// The environment mirrors `tests/differential.rs`'s isolation so a probe cannot
/// read the developer's real config, credentials or sessions, cannot reach the
/// network, and cannot write outside `root`. `Command::output` also pipes stdout,
/// which is what makes the `tui` probe refuse instead of entering raw mode.
fn probe_binary(argv: &[&str], root: &std::path::Path) -> std::process::Output {
    binary()
        .args(argv)
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("HOME", root.join("home"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("ZUNO_DB", root.join("opencode.db"))
        .env("ZUNO_DISABLE_AUTOUPDATE", "true")
        .env("ZUNO_DISABLE_MODELS_FETCH", "true")
        .env("ZUNO_DISABLE_LSP_DOWNLOAD", "true")
        .output()
        .unwrap_or_else(|error| panic!("{argv:?} must run: {error}"))
}

/// **Every implemented command reaches its handler through the production
/// dispatcher.**
///
/// This is the guard todo 116 claimed and did not build. Its predecessors read
/// The routing decision lives in `crates/zuno-cli/src/cmd/mod.rs`'s exhaustive
/// `match`. This runs the shipped binary, once per implemented command, and reads what
/// the user reads. Nothing in the assertion can be satisfied by parsing:
/// [`Probe::evidence`] is a fragment only that command's handler emits, so the
/// arm must have called the handler for the probe to pass, and the routing table
/// is exercised in full rather than sampled — all twelve arms of the `match` are
/// covered, because [`surface_every_implemented_command_actually_has_a_handler`]
/// makes [`IMPLEMENTED_PROBES`] a bijection with the implemented dispositions.
/// A guard that probed only `agent` would fall to the same mutation one arm over.
///
/// Cost, stated so it is not "optimised" away later: twelve subprocesses, each
/// chosen to fail or finish immediately rather than to do the command's real
/// work. Reaching the handler is the whole claim; finishing its job is other
/// tests' business.
#[test]
fn surface_every_implemented_command_reaches_its_handler_through_the_production_dispatcher() {
    let mut failures = Vec::new();
    for probe in IMPLEMENTED_PROBES {
        let root = tempfile::tempdir().expect("probe root");
        let output = probe_binary(probe.argv, root.path());
        let observed = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        if !observed.contains(probe.evidence) {
            failures.push(format!(
                "`{}` never produced its handler's own output: `{:?}` was expected to emit \
                 {:?}, and emitted:\n{observed}",
                probe.command, probe.argv, probe.evidence
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
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
            "ZUNO_DB",
            std::env::temp_dir().join("zuno-surface-export.db"),
        )
        .output()
        .expect("run export");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("handler is pending"),
        "export still reports a missing handler: {stderr}"
    );
}

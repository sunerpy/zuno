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
    /// [`oc_cli::PendingCommandDispatcher`] emits one fixed sentence and produces
    /// no other output, so observing this fragment is positive evidence that the
    /// production match arm ran the handler. Asserting only the *absence* of the
    /// pending sentence would also pass if the arm were replaced with `Ok(())`.
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

/// Resolve one argv to the dispatch request the CLI would hand a handler.
fn dispatch_request(argv: &[&str]) -> Box<oc_cli::DispatchRequest> {
    let cli = Cli::try_parse_from(std::iter::once("opencode-rust").chain(argv.iter().copied()))
        .unwrap_or_else(|error| panic!("{argv:?} must parse: {error}"));
    match cli.action(&oc_paths::Env::empty()) {
        Action::Dispatch(request) => request,
        other => panic!("{argv:?} must dispatch, got {other:?}"),
    }
}

/// **No command advertised as implemented may *parse* into the pending variant.**
///
/// Scope, stated precisely because todo 116 overstated it: this reads the
/// [`oc_cli::DispatchArguments`] variant produced by parsing, which is one step
/// short of the routing decision. It catches a command registered straight onto
/// [`oc_cli::DispatchArguments::Pending`] — the shape `completion` has — and it
/// cannot catch a `match` arm in `cmd/mod.rs` that hands a non-`Pending` variant
/// to [`oc_cli::PendingCommandDispatcher`] anyway. That mutation kept this test
/// green while `agent list` exited 1, which is why
/// [`surface_every_implemented_command_reaches_its_handler_through_the_production_dispatcher`]
/// exists and drives the real binary instead.
#[test]
fn surface_no_implemented_disposition_parses_into_the_pending_variant() {
    let mut liars = Vec::new();
    for entry in dispositions() {
        if entry.disposition != Disposition::Implemented {
            continue;
        }
        let Some(probe) = IMPLEMENTED_PROBES
            .iter()
            .find(|probe| probe.command == entry.command)
        else {
            continue;
        };
        if dispatch_request(probe.argv).args.is_pending() {
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
            IMPLEMENTED_PROBES
                .iter()
                .any(|probe| &probe.command == command),
            "`{command}` is implemented but has no probe in IMPLEMENTED_PROBES"
        );
    }
    for probe in IMPLEMENTED_PROBES {
        assert!(
            implemented.contains(&probe.command),
            "IMPLEMENTED_PROBES names `{}`, which is not an implemented disposition",
            probe.command
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
        .map(|probe| probe.argv[0])
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
        .env("ZUNO_DISABLE_DEFAULT_PLUGINS", "true")
        .env("ZUNO_DISABLE_LSP_DOWNLOAD", "true")
        .output()
        .unwrap_or_else(|error| panic!("{argv:?} must run: {error}"))
}

/// The two sentences [`oc_cli::PendingCommandDispatcher`] can print for `command`.
///
/// Anchored on the command name so the check cannot be tripped by a handler that
/// happens to say "is not available" about something else — `run --fork` reports
/// "a session-history fork API that is not available yet", which is a handler
/// speaking, not the stub.
fn pending_markers(command: &str) -> [String; 2] {
    [
        format!("`{command}` is registered, but its handler is pending"),
        format!("`{command}` is not available:"),
    ]
}

/// **Every implemented command reaches its handler through the production
/// dispatcher.**
///
/// This is the guard todo 116 claimed and did not build. Its predecessors read
/// [`oc_cli::DispatchArguments::is_pending`], which answers "what did parsing
/// produce"; the routing decision is one step later, in
/// `crates/oc-cli/src/cmd/mod.rs`'s `match`. F2 proved the difference by editing
/// that `match` so `DispatchArguments::Agent(_)` went to
/// [`oc_cli::PendingCommandDispatcher`]: every surface test stayed green while
/// `agent list` printed "`agent` is registered, but its handler is pending" and
/// exited 1.
///
/// So this runs the shipped binary, once per implemented command, and reads what
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

        for marker in pending_markers(probe.command) {
            if observed.contains(&marker) {
                failures.push(format!(
                    "`{}` is recorded as implemented, but `{:?}` reached the pending handler in \
                     production:\n{observed}",
                    probe.command, probe.argv
                ));
            }
        }
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

/// The negative control for the guard above, driven through the real binary.
///
/// [`surface_failure_scenario_the_pending_handler_is_detectable`] proves
/// [`oc_cli::PendingCommandDispatcher`] fails when called directly. This proves
/// the *binary* still surfaces that failure in the stream the guard above reads,
/// so "no probe printed a pending marker" is a live observation and not a string
/// that nothing can produce any more. `completion` is the one command legitimately
/// routed there.
#[test]
fn surface_failure_scenario_the_binary_prints_a_pending_marker_for_a_stub() {
    let root = tempfile::tempdir().expect("probe root");
    let output = probe_binary(&["completion"], root.path());
    assert!(!output.status.success());
    let observed = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        pending_markers("completion")
            .iter()
            .any(|marker| observed.contains(marker)),
        "the guard's pending markers no longer match what a stub prints: {observed}"
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
        .env("ZUNO_DB", std::env::temp_dir().join("oc-surface-export.db"))
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

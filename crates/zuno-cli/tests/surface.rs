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
    assert!(registered.contains(&"acp"));
    assert!(registered.contains(&"providers"));
    assert!(registered.contains(&"plugin"));
}

#[test]
fn surface_self_update_replaces_the_rejected_upgrade_placeholder() {
    let command = Cli::command();
    assert!(
        command.find_subcommand("upgrade").is_none(),
        "the unreleased Rust CLI must not retain the rejected compatibility placeholder"
    );

    let self_update = command
        .find_subcommand("self-update")
        .expect("self-update must be a real top-level command");
    let flags: Vec<&str> = self_update
        .get_arguments()
        .filter_map(clap::Arg::get_long)
        .collect();
    for required in ["check", "force", "tag", "yes"] {
        assert!(
            flags.contains(&required),
            "self-update is missing --{required}; flags: {flags:?}"
        );
    }

    let disposition = disposition_for("self-update").expect("self-update disposition");
    assert_eq!(disposition.disposition, Disposition::Implemented);
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
// The leaf inventory: every invocation the parser routes, and the handler that
// answers it
// ---------------------------------------------------------------------------

/// What proves *this* leaf's handler ran.
#[derive(Debug, Clone, Copy)]
enum Evidence {
    /// A fragment of the handler's own output, on either stream.
    ///
    /// Observing it is positive evidence that the production match arm called the
    /// handler. Asserting only a successful exit would also pass if the arm were
    /// replaced with `Ok(())`.
    Fragment(&'static str),
    /// The handler declines silently, and the absence of output is the observable.
    ///
    /// `debug snapshot track` is the only such leaf. `Store::enabled()` is
    /// `location.git && location.enabled`, so outside a version-controlled project
    /// the capture returns `Ok(None)` and prints nothing, while inside one it prints
    /// a freshly computed tree hash that no fixture can name. Both halves of the
    /// claim carry weight: a leaf that refuses for want of a handler prints its
    /// refusal, and any output at all fails this assertion.
    SilentSuccess,
}

/// One dispatchable invocation, and the observable that proves its handler ran.
struct Leaf {
    /// The subcommand path, exactly as the parser spells it.
    ///
    /// [`surface_every_dispatchable_leaf_is_inventoried`] matches these against the
    /// walked command tree, so a new subcommand cannot be registered without
    /// naming the evidence that its handler exists.
    path: &'static [&'static str],
    /// An invocation that reaches the handler and returns promptly, touching no
    /// network and writing nothing outside the probe's temporary root.
    ///
    /// It must start with `path`. Several leaves are entered through an argument the
    /// handler itself refuses, because that refusal is produced *inside* the
    /// handler, after routing, and before it does anything expensive: a bare `serve`
    /// would block on a listening socket and a bare `run` would want a provider.
    argv: &'static [&'static str],
    evidence: Evidence,
}

/// Every dispatchable leaf of the shipped command tree.
///
/// The rejected commands are here too, entered by name, because "registered and
/// deliberately refused" is a behavior with an owner and a message rather than an
/// absence: [`surface_no_dispatchable_leaf_refuses_for_want_of_a_handler`] exempts
/// exactly the [`Disposition::Rejected`] rows and nothing else.
///
/// Three pairs of leaves share their observable, and it is a property of the code
/// rather than of the probe. `plugin add` and `plugin update` are one `install`
/// handler distinguished only by an `InstallMode` a caller cannot see,
/// `debug snapshot patch` and `debug snapshot diff` both fail in the shared store
/// preflight before they diverge, and `debug lsp diagnostics` and
/// `debug lsp document-symbols` both stop at the same "no language server" lookup.
/// Each still proves that its own leaf routes into that handler, which is the claim
/// this inventory makes.
const LEAVES: &[Leaf] = &[
    Leaf {
        path: &["acp"],
        argv: &["acp", "--check"],
        evidence: Evidence::Fragment("ACP stdio adapter ready"),
    },
    Leaf {
        path: &["run"],
        argv: &[
            "run",
            "--continue",
            "--session",
            "ses_probe000000000000000000000a",
            "probe",
        ],
        evidence: Evidence::Fragment("--continue and --session cannot be used together"),
    },
    Leaf {
        path: &["tui"],
        argv: &["tui"],
        evidence: Evidence::Fragment("the interactive TUI requires a terminal"),
    },
    Leaf {
        // The refusal is `zuno-server`'s: a non-loopback bind with no
        // `ZUNO_SERVER_PASSWORD` is rejected before the listener is created, which
        // is the one `serve` invocation that reaches the handler and returns.
        path: &["serve"],
        argv: &["serve", "--hostname", "0.0.0.0"],
        evidence: Evidence::Fragment("a non-loopback listener would expose the unauthenticated"),
    },
    Leaf {
        path: &["session", "list"],
        argv: &["session", "list", "--project", "not-a-project"],
        evidence: Evidence::Fragment("Project not found: not-a-project"),
    },
    Leaf {
        path: &["session", "prune"],
        argv: &[
            "session",
            "prune",
            "--older-than",
            "3650",
            "--format",
            "json",
        ],
        evidence: Evidence::Fragment("\"action\":\"preview\""),
    },
    Leaf {
        path: &["session", "delete"],
        argv: &[
            "session",
            "delete",
            "ses_probe000000000000000000000a",
            "--keep-derived-experiences",
        ],
        evidence: Evidence::Fragment("Session not found: ses_probe000000000000000000000a"),
    },
    Leaf {
        path: &["agent", "list"],
        argv: &["agent", "list"],
        evidence: Evidence::Fragment("build (primary)"),
    },
    Leaf {
        path: &["models"],
        argv: &["models", "--refresh"],
        evidence: Evidence::Fragment("the model catalog cannot be refreshed"),
    },
    Leaf {
        path: &["providers", "list"],
        argv: &["providers", "list"],
        evidence: Evidence::Fragment("0 credentials"),
    },
    Leaf {
        // An unknown provider is refused by the helper `login` shares, so the
        // fragment names the refusal rather than the listing this cannot reach
        // without a configured provider.
        path: &["providers", "methods"],
        argv: &["providers", "methods", "not-a-provider"],
        evidence: Evidence::Fragment("provider \"not-a-provider\" has no configured login"),
    },
    Leaf {
        path: &["providers", "login"],
        argv: &["providers", "login", "probe", "--provider", "probe"],
        evidence: Evidence::Fragment("a positional provider cannot be combined with --provider"),
    },
    Leaf {
        path: &["providers", "logout"],
        argv: &["providers", "logout", "not-a-provider"],
        evidence: Evidence::Fragment("No credentials found"),
    },
    Leaf {
        path: &["mcp", "add"],
        argv: &["mcp", "add", "probe"],
        evidence: Evidence::Fragment("Provide either --url <url> or a command after --"),
    },
    Leaf {
        path: &["mcp", "list"],
        argv: &["mcp", "list"],
        evidence: Evidence::Fragment("No MCP servers configured"),
    },
    Leaf {
        path: &["mcp", "auth"],
        argv: &["mcp", "auth"],
        evidence: Evidence::Fragment("MCP server name is required in non-interactive mode"),
    },
    Leaf {
        path: &["mcp", "auth", "list"],
        argv: &["mcp", "auth", "list"],
        evidence: Evidence::Fragment("No OAuth-capable MCP servers configured"),
    },
    Leaf {
        path: &["mcp", "logout"],
        argv: &["mcp", "logout", "not-a-server"],
        evidence: Evidence::Fragment("No MCP OAuth credentials stored"),
    },
    Leaf {
        path: &["mcp", "debug"],
        argv: &["mcp", "debug", "not-a-server"],
        evidence: Evidence::Fragment("MCP server not found: not-a-server"),
    },
    Leaf {
        path: &["plugin", "list"],
        argv: &["plugin", "list"],
        evidence: Evidence::Fragment("No plugins active for"),
    },
    Leaf {
        path: &["plugin", "add"],
        argv: &["plugin", "add", "probe-package"],
        evidence: Evidence::Fragment("failed to read"),
    },
    Leaf {
        path: &["plugin", "update"],
        argv: &["plugin", "update", "probe-package"],
        evidence: Evidence::Fragment("failed to read"),
    },
    Leaf {
        path: &["plugin", "remove"],
        argv: &["plugin", "remove", "probe.missing"],
        evidence: Evidence::Fragment("plugin package `probe.missing` is not installed"),
    },
    Leaf {
        path: &["db"],
        argv: &["db", "--format", "json", "select 1 as probe"],
        evidence: Evidence::Fragment("\"probe\": 1"),
    },
    Leaf {
        path: &["debug", "paths"],
        argv: &["debug", "paths"],
        evidence: Evidence::Fragment("repos"),
    },
    Leaf {
        path: &["debug", "config"],
        argv: &["debug", "config"],
        evidence: Evidence::Fragment("\"agents\": {}"),
    },
    Leaf {
        path: &["debug", "agent"],
        argv: &["debug", "agent", "not-an-agent"],
        evidence: Evidence::Fragment("Agent not found: not-an-agent"),
    },
    Leaf {
        path: &["debug", "prompt"],
        argv: &["debug", "prompt"],
        evidence: Evidence::Fragment("no prompt receipt found in the database"),
    },
    Leaf {
        path: &["debug", "permissions"],
        argv: &["debug", "permissions"],
        evidence: Evidence::Fragment("\"allowAllStillEnforces\""),
    },
    Leaf {
        path: &["debug", "skill"],
        argv: &["debug", "skill"],
        evidence: Evidence::Fragment("\"disabledSources\""),
    },
    Leaf {
        // `danger-full-access` resolves through the native backend on every
        // platform, so the report is the same shape on a host without bubblewrap.
        path: &["debug", "sandbox"],
        argv: &["debug", "sandbox", "--mode", "danger-full-access"],
        evidence: Evidence::Fragment("\"requestedMode\": \"danger-full-access\""),
    },
    Leaf {
        path: &["debug", "rg", "files"],
        argv: &["debug", "rg", "files", "--glob", "probe[", "--limit", "1"],
        evidence: Evidence::Fragment("invalid glob pattern probe["),
    },
    Leaf {
        path: &["debug", "rg", "search"],
        argv: &["debug", "rg", "search", "probe(", "--limit", "1"],
        evidence: Evidence::Fragment("invalid regex pattern probe("),
    },
    Leaf {
        path: &["debug", "lsp", "diagnostics"],
        argv: &["debug", "lsp", "diagnostics", "probe.rs"],
        evidence: Evidence::Fragment("no language server is available for"),
    },
    Leaf {
        // An offline registry resolves no server, so the handler's own observable
        // is the empty symbol document it prints.
        path: &["debug", "lsp", "symbols"],
        argv: &["debug", "lsp", "symbols", "probe"],
        evidence: Evidence::Fragment("[]"),
    },
    Leaf {
        // The argument is workspace-relative, and deliberately not a `file://` URI:
        // `resolve_path` sends a URI through `Url::to_file_path`, whose Windows
        // implementation accepts only a drive-rooted first segment, so any URI a
        // fixture could spell here is either rejected on Windows or rejected on
        // Unix. That branch is platform-specific and is pinned by
        // `paths_accept_file_uris_and_workspace_relative_values` in `cmd/debug.rs`,
        // which builds the URI from a real path. This leaf's own claim is routing,
        // and it shares `diagnostics`' observable because both arguments reach the
        // same `LspManager` lookup; the file name differs so a failure report says
        // which probe produced it.
        path: &["debug", "lsp", "document-symbols"],
        argv: &[
            "debug",
            "lsp",
            "document-symbols",
            "document-symbols-probe.rs",
        ],
        evidence: Evidence::Fragment("no language server is available for"),
    },
    Leaf {
        path: &["debug", "snapshot", "track"],
        argv: &["debug", "snapshot", "track"],
        evidence: Evidence::SilentSuccess,
    },
    Leaf {
        path: &["debug", "snapshot", "patch"],
        argv: &["debug", "snapshot", "patch", "deadbeef"],
        evidence: Evidence::Fragment("not a git repository"),
    },
    Leaf {
        path: &["debug", "snapshot", "diff"],
        argv: &["debug", "snapshot", "diff", "deadbeef"],
        evidence: Evidence::Fragment("not a git repository"),
    },
    Leaf {
        path: &["completion"],
        argv: &["completion", "bash"],
        evidence: Evidence::Fragment("_zuno"),
    },
    Leaf {
        path: &["self-update"],
        argv: &["self-update", "--tag", "not-a-version"],
        evidence: Evidence::Fragment("invalid release version"),
    },
    Leaf {
        path: &["export"],
        argv: &["export", "ses_probe000000000000000000000a"],
        evidence: Evidence::Fragment("Exported Zuno bundle:"),
    },
    Leaf {
        path: &["import"],
        argv: &["import", "probe.json"],
        evidence: Evidence::Fragment("Bundle not found: probe.json"),
    },
    Leaf {
        path: &["console"],
        argv: &["console"],
        evidence: Evidence::Fragment("Zuno does not provide a hosted console"),
    },
    Leaf {
        path: &["web"],
        argv: &["web"],
        evidence: Evidence::Fragment("the bundled hosted web application is excluded"),
    },
    Leaf {
        path: &["stats"],
        argv: &["stats"],
        evidence: Evidence::Fragment("use `db stats`"),
    },
    Leaf {
        path: &["github"],
        argv: &["github"],
        evidence: Evidence::Fragment("the hosted GitHub agent is outside the local-agent scope"),
    },
    Leaf {
        path: &["pr"],
        argv: &["pr"],
        evidence: Evidence::Fragment("use `gh pr checkout <number>`"),
    },
    Leaf {
        path: &["uninstall"],
        argv: &["uninstall"],
        evidence: Evidence::Fragment("self-uninstallation is excluded from the runtime"),
    },
    Leaf {
        path: &["generate"],
        argv: &["generate"],
        evidence: Evidence::Fragment("use the server's `/openapi.json` document instead"),
    },
];

/// Every invocation the parser will route to a handler, as a full subcommand path.
///
/// # Why the walk is recursive
///
/// `agent create` shipped registered over a handler whose only possible outcome was
/// a refusal, and the inventory that existed to prevent exactly that compared only
/// `Cli::command().get_subcommands()` — the top level, where `agent` is present and
/// honest. A nested command could therefore never fail it. This walks to the leaves,
/// so registering `<parent> <child>` over an absent handler now fails a test.
///
/// A node is dispatchable when the parser accepts it as the last subcommand of an
/// invocation: either it has no subcommands, or it has subcommands and does not
/// require one. That is why `mcp auth` appears here alongside `mcp auth list` — it
/// prints its own status when named bare. clap's generated `help` is not a Zuno
/// command. The root is dispatchable as well, since a bare `zuno` is the TUI, and it
/// is covered by the `tui` leaf under its own name.
fn dispatchable_paths(command: &clap::Command) -> Vec<Vec<&str>> {
    fn walk<'a>(node: &'a clap::Command, prefix: &[&'a str], out: &mut Vec<Vec<&'a str>>) {
        let children: Vec<&'a clap::Command> = node
            .get_subcommands()
            .filter(|child| child.get_name() != "help")
            .collect();
        if (children.is_empty() || !node.is_subcommand_required_set()) && !prefix.is_empty() {
            out.push(prefix.to_vec());
        }
        for child in children {
            let mut path = prefix.to_vec();
            path.push(child.get_name());
            walk(child, &path, out);
        }
    }

    let mut paths = Vec::new();
    walk(command, &[], &mut paths);
    paths
}

/// **Every dispatchable leaf of the command tree is inventoried, and nothing else
/// is.**
#[test]
fn surface_every_dispatchable_leaf_is_inventoried() {
    let command = Cli::command();
    let walked = dispatchable_paths(&command);
    let inventoried: Vec<Vec<&str>> = LEAVES.iter().map(|leaf| leaf.path.to_vec()).collect();

    let unproven: Vec<String> = walked
        .iter()
        .filter(|path| !inventoried.contains(path))
        .map(|path| path.join(" "))
        .collect();
    assert!(
        unproven.is_empty(),
        "these invocations are routed with nothing proving a handler answers them: {unproven:?}"
    );

    let stale: Vec<String> = inventoried
        .iter()
        .filter(|path| !walked.contains(path))
        .map(|path| path.join(" "))
        .collect();
    assert!(
        stale.is_empty(),
        "these inventory rows name invocations the parser no longer routes: {stale:?}"
    );

    for leaf in LEAVES {
        assert!(
            leaf.argv.starts_with(leaf.path),
            "`{:?}` does not invoke `{}`",
            leaf.argv,
            leaf.path.join(" ")
        );
        assert_eq!(
            inventoried
                .iter()
                .filter(|path| path.as_slice() == leaf.path)
                .count(),
            1,
            "`{}` is inventoried more than once",
            leaf.path.join(" ")
        );
    }
    assert_eq!(walked.len(), LEAVES.len());
}

/// **No probe spells a path only one platform accepts, and no evidence quotes a
/// resolved one.**
///
/// This inventory runs on Linux, macOS and Windows CI, so a fixture that encodes a
/// POSIX path passes where it was written and fails where it was not. Both halves
/// have a concrete failure: an argument like `file:///probe.rs` reaches
/// `resolve_path`, whose `Url::to_file_path` accepts only a drive-rooted first
/// segment on Windows and answers `invalid file URI` there, and a fragment that
/// quotes the path the handler resolved cannot match on a host that spells the
/// separator `\`.
///
/// A fragment may contain `/` inside a code span, because that is Zuno's own markup
/// for a route or a spelling — `generate` points at `/openapi.json` — rather than a
/// path the handler resolved on the running host.
#[test]
fn surface_no_probe_or_evidence_spells_a_platform_specific_path() {
    fn drive_rooted(value: &str) -> bool {
        let bytes = value.as_bytes();
        bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
    }

    fn outside_code_spans(fragment: &str) -> String {
        fragment
            .split('`')
            .step_by(2)
            .collect::<Vec<&str>>()
            .join(" ")
    }

    let mut offences = Vec::new();
    for leaf in LEAVES {
        let leaf_name = leaf.path.join(" ");
        for argument in leaf.argv {
            let rooted = argument.starts_with('/')
                || argument.contains("file://")
                || argument.contains('\\')
                || drive_rooted(argument);
            if rooted {
                offences.push(format!(
                    "`{leaf_name}` probes with {argument:?}, which names a path only one \
                     platform accepts"
                ));
            }
        }
        if let Evidence::Fragment(fragment) = leaf.evidence {
            let prose = outside_code_spans(fragment);
            if prose.contains('/') || prose.contains('\\') || drive_rooted(&prose) {
                offences.push(format!(
                    "`{leaf_name}` expects {fragment:?}, which quotes a path the handler \
                     resolved; the separator differs by platform"
                ));
            }
        }
    }
    assert!(offences.is_empty(), "{}", offences.join("\n"));
}

/// `agent create` is gone, rather than registered over a refusal.
///
/// Writing an agent definition is editing a Markdown file under `.zuno/agent/`,
/// which `docs/config/custom-agents.md` documents. The subcommand promised a
/// model-backed generator that does not exist, and every invocation of it ended in
/// the same message.
#[test]
fn surface_agent_create_is_not_registered() {
    let command = Cli::command();
    let agent = command
        .find_subcommand("agent")
        .expect("agent must be registered");
    assert!(
        agent.find_subcommand("create").is_none(),
        "`agent create` is registered again; its handler must exist before its help text does"
    );
    let subcommands: Vec<&str> = agent
        .get_subcommands()
        .map(clap::Command::get_name)
        .filter(|name| *name != "help")
        .collect();
    assert_eq!(subcommands, vec!["list"]);
}

/// Every disposition-table command owns at least one probed leaf.
///
/// The table and the parser are compared by
/// [`surface_registered_commands_match_their_dispositions`]; this is the second
/// half, so a registered command cannot become a help-text entry with no probe.
#[test]
fn surface_every_registered_disposition_owns_a_probed_leaf() {
    for entry in dispositions() {
        let probed = LEAVES
            .iter()
            .any(|leaf| leaf.path.first() == Some(&entry.command));
        match entry.disposition {
            Disposition::Implemented | Disposition::Rejected => assert!(
                probed,
                "`{}` is {:?} and must own a probed leaf",
                entry.command, entry.disposition
            ),
            Disposition::NotRegistered => assert!(
                !probed,
                "`{}` is explicitly deferred and must not be probed",
                entry.command
            ),
        }
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
        .current_dir(root)
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
        .env_remove("ZUNO_SERVER_PASSWORD")
        .output()
        .unwrap_or_else(|error| panic!("{argv:?} must run: {error}"))
}

/// One leaf's probe, as the user would read it.
struct Observation {
    leaf: &'static Leaf,
    output: String,
    success: bool,
}

/// Run every inventoried probe once.
///
/// Cost, stated so it is not "optimised" away later: one subprocess per
/// dispatchable leaf, each chosen to fail or finish immediately rather than to do
/// the command's real work. Reaching the handler is the whole claim; finishing its
/// job is other tests' business. The probes are independent, so they run on a few
/// threads and both guards below share one pass.
fn observe_leaves() -> Vec<Observation> {
    const WORKERS: usize = 8;

    let next = std::sync::atomic::AtomicUsize::new(0);
    let observed = std::sync::Mutex::new(Vec::with_capacity(LEAVES.len()));
    std::thread::scope(|scope| {
        for _ in 0..WORKERS {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(leaf) = LEAVES.get(index) else {
                        return;
                    };
                    let root = tempfile::tempdir().expect("probe root");
                    let output = probe_binary(leaf.argv, root.path());
                    observed
                        .lock()
                        .expect("probe observations")
                        .push(Observation {
                            leaf,
                            output: format!(
                                "{}{}",
                                String::from_utf8_lossy(&output.stdout),
                                String::from_utf8_lossy(&output.stderr)
                            ),
                            success: output.status.success(),
                        });
                }
            });
        }
    });
    observed.into_inner().expect("probe observations")
}

/// **Every dispatchable leaf reaches its handler through the production
/// dispatcher.**
///
/// The routing decision lives in `crates/zuno-cli/src/cmd/mod.rs`'s exhaustive
/// `match` and in each command's own nested match. This runs the shipped binary,
/// once per leaf, and reads what the user reads. Nothing in the assertion can be
/// satisfied by parsing: [`Evidence::Fragment`] names output only that handler
/// emits, so the arm must have called it, and the routing tables are exercised in
/// full rather than sampled because
/// [`surface_every_dispatchable_leaf_is_inventoried`] makes [`LEAVES`] a bijection
/// with the walked tree. A guard that probed only `agent` would fall to the same
/// mutation one arm over.
#[test]
fn surface_every_dispatchable_leaf_reaches_its_handler() {
    let mut failures = Vec::new();
    for observation in observe_leaves() {
        let leaf = observation.leaf;
        match leaf.evidence {
            Evidence::Fragment(fragment) => {
                if !observation.output.contains(fragment) {
                    failures.push(format!(
                        "`{}` never produced its handler's own output: `{:?}` was expected to \
                         emit {fragment:?}, and emitted:\n{}",
                        leaf.path.join(" "),
                        leaf.argv,
                        observation.output
                    ));
                }
            }
            Evidence::SilentSuccess => {
                if !observation.output.is_empty() || !observation.success {
                    failures.push(format!(
                        "`{}` is inventoried as declining silently, and `{:?}` exited \
                         success={} emitting:\n{}",
                        leaf.path.join(" "),
                        leaf.argv,
                        observation.success,
                        observation.output
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// The refusal vocabulary that means "this behavior does not exist here".
///
/// These are the phrasings Zuno has actually used for a registered surface with no
/// implementation behind it, as opposed to a refusal of the caller's input, which
/// is a handler doing its job. `Disposition::Rejected` commands own the same
/// vocabulary on purpose and are exempt.
///
/// Every phrase is lowercase, because [`absent_capability_phrase`] lowercases the
/// text it searches.
const ABSENT_CAPABILITY_PHRASES: &[&str] = &[
    "is not available yet",
    "is not available in the local rust runtime",
    "is not supported yet",
    "is not supported by the rust",
    "is not implemented",
    "not implemented yet",
    "handler is pending",
];

fn absent_capability_phrase(text: &str) -> Option<&'static str> {
    let text = text.to_lowercase();
    ABSENT_CAPABILITY_PHRASES
        .iter()
        .copied()
        .find(|phrase| text.contains(*phrase))
}

/// **No leaf outside the rejected set answers with "that does not exist here".**
///
/// This is the half of the contract that `agent create` escaped. Registration and a
/// probe are not enough on their own: a subcommand whose handler can only report
/// the absence of the thing it names would satisfy both, and did. A command that
/// cannot be implemented yet belongs in [`Disposition::NotRegistered`] until it
/// can, and a command Zuno will not implement belongs in
/// [`Disposition::Rejected`], where the refusal is the documented behavior.
#[test]
fn surface_no_dispatchable_leaf_refuses_for_want_of_a_handler() {
    let mut absent = Vec::new();
    for observation in observe_leaves() {
        let leaf = observation.leaf;
        let deliberately_rejected = leaf
            .path
            .first()
            .and_then(|command| disposition_for(command))
            .is_some_and(|entry| entry.disposition == Disposition::Rejected);
        if deliberately_rejected {
            continue;
        }
        if let Some(phrase) = absent_capability_phrase(&observation.output) {
            absent.push(format!(
                "`{}` is registered over an absent capability: `{:?}` answered {phrase:?}:\n{}",
                leaf.path.join(" "),
                leaf.argv,
                observation.output
            ));
        }
    }
    assert!(absent.is_empty(), "{}", absent.join("\n\n"));
}

/// The vocabulary detector recognizes every refusal Zuno has shipped over an
/// absent handler.
///
/// Named after the defects rather than after a mechanism, so the guard above cannot
/// quietly stop matching the thing it was built for. Each string was produced by a
/// registered surface that could do nothing else.
#[test]
fn surface_failure_scenario_shipped_absent_handler_refusals_are_detected() {
    const SHIPPED_REFUSALS: &[&str] = &[
        "agent creation requires the model-backed generator, which is not available yet",
        "--fork requires a session-history fork API that is not available yet",
        "--share is not available in the local Rust runtime",
        "--attach/--port/--username/--password require the remote SDK client, which is not \
         available yet",
        "--mdns is not supported by the Rust server runtime yet",
        "--mdns-domain requires --mdns, which is not supported yet",
        "--cors is not supported by the Rust server runtime yet",
        "`export` is registered, but its handler is pending todo 56",
    ];

    for shipped in SHIPPED_REFUSALS {
        assert!(
            absent_capability_phrase(shipped).is_some(),
            "the detector no longer recognizes a refusal Zuno shipped: {shipped:?}"
        );
    }
    assert!(
        absent_capability_phrase("Session not found: ses_probe000000000000000000000a").is_none(),
        "refusing the caller's input is a handler doing its job"
    );
    assert!(
        absent_capability_phrase("no language server is available for probe.rs").is_none(),
        "an absent external dependency is not an absent handler"
    );
}

/// `export` in particular no longer reports a missing handler.
///
/// Named after the defect rather than after a mechanism, so the reproduction from
/// the F3 verification report section 9 stays executable: this is the exact
/// invocation that printed "`export` is registered, but its handler is pending
/// todo 56" and exited 1.
#[test]
fn surface_export_no_longer_reports_a_pending_handler() {
    let root = tempfile::tempdir().expect("probe root");
    let output = binary()
        .arg("export")
        .arg(root.path().join("probe.zuno-bundle"))
        .current_dir(root.path())
        .env("HOME", root.path().join("home"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("ZUNO_DB", root.path().join("zuno-surface-export.db"))
        .output()
        .expect("run export");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("handler is pending"),
        "export still reports a missing handler: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// The documentation half of the surface contract
// ---------------------------------------------------------------------------

/// Every Markdown page under `docs/`, as a repo-relative path and its text.
///
/// Read from disk rather than through `include_str!`, because the pages that go
/// stale after a removal are the ones nobody thought to name: the guard that
/// shipped with these removals listed the two reference pages its author had just
/// edited, and the four guide pages that still promised `--fork` were outside it by
/// construction. A walk cannot be out of date.
fn documentation_pages() -> Vec<(String, String)> {
    fn walk(directory: &std::path::Path, root: &std::path::Path, out: &mut Vec<(String, String)>) {
        let entries = std::fs::read_dir(directory)
            .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()));
        for entry in entries {
            let path = entry.expect("documentation directory entry").path();
            if path.is_dir() {
                walk(&path, root, out);
            } else if path.extension().is_some_and(|extension| extension == "md") {
                let relative = path
                    .strip_prefix(root)
                    .expect("documentation page is under the workspace root")
                    .to_string_lossy()
                    .replace('\\', "/");
                let text = std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
                out.push((relative, text));
            }
        }
    }

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("zuno-cli is under <workspace>/crates")
        .to_path_buf();
    let mut pages = Vec::new();
    walk(&root.join("docs"), &root, &mut pages);
    pages.sort_by(|left, right| left.0.cmp(&right.0));
    assert!(
        pages.len() > 100,
        "the documentation walk found only {} pages; it is looking in the wrong place",
        pages.len()
    );
    pages
}

/// The eight options `zuno run` accepted and could only refuse.
const REMOVED_RUN_OPTIONS: &[&str] = &[
    "--fork",
    "--share",
    "--attach",
    "--port",
    "--username",
    "--password",
    "--interactive",
    "--auto",
];

/// **No page anywhere offers an option `zuno run` no longer accepts.**
///
/// Documentation is part of the surface contract: an option row or a copyable
/// example is a promise that the flag works. Three rules, because the eight
/// removals are not all the same shape.
///
/// A `zuno run` command line, on any page, may not name any of the eight — that is
/// what a reader copies. The two `run` reference pages may not carry an option row
/// for one either, which is the table a reader trusts over the prose. And the four
/// that no Zuno command accepts at all — `--fork`, `--share`, `--attach`,
/// `--interactive` — may appear only on the one page per language that records the
/// removal, because a mention anywhere else is a page that has not been updated.
/// `--port` and `--auto` are excluded from that last rule: `zuno serve --port` and
/// `zuno tui --auto` are real.
#[test]
fn surface_documentation_never_offers_a_run_option_the_parser_rejects() {
    /// The one page per language that says these options were removed.
    const REMOVAL_RECORD: &[&str] = &["docs/cli/run.md", "docs/zh/cli/run.md"];
    /// The subset no Zuno command accepts under any name.
    const RETIRED_OUTRIGHT: &[&str] = &["--fork", "--share", "--attach", "--interactive"];

    let mut offences = Vec::new();
    for (page, text) in documentation_pages() {
        let records_the_removal = REMOVAL_RECORD.contains(&page.as_str());
        let mut invocation = false;
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            let number = index + 1;
            invocation = invocation || line.contains("zuno run");
            for option in REMOVED_RUN_OPTIONS {
                if invocation && line.contains(option) {
                    offences.push(format!("{page}:{number} runs {option}: {line}"));
                }
                if records_the_removal && line.starts_with("| `") && line.contains(option) {
                    offences.push(format!("{page}:{number} tabulates {option}: {line}"));
                }
            }
            if !records_the_removal {
                for option in RETIRED_OUTRIGHT {
                    if line.contains(option) {
                        offences.push(format!("{page}:{number} mentions {option}: {line}"));
                    }
                }
            }
            invocation = invocation && line.ends_with('\\');
        }
    }
    assert!(offences.is_empty(), "{}", offences.join("\n"));
}

/// **The CLI reference never offers `zuno agent create`.**
///
/// Authoring an agent is writing a Markdown file under `.zuno/agent/`, and the
/// subcommand that claimed to generate one is gone. This is the reference tree,
/// where a removed subcommand would otherwise keep its synopsis, its option table
/// and its worked example long after the parser stopped accepting it.
#[test]
fn surface_cli_reference_never_offers_the_removed_agent_create() {
    let mut offences = Vec::new();
    for (page, text) in documentation_pages() {
        if !page.starts_with("docs/cli/") && !page.starts_with("docs/zh/cli/") {
            continue;
        }
        for (index, line) in text.lines().enumerate() {
            if line.contains("agent create") {
                offences.push(format!("{page}:{} offers {line}", index + 1));
            }
        }
    }
    assert!(offences.is_empty(), "{}", offences.join("\n"));
}

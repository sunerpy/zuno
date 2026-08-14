//! **Every implemented command's output, compared against the released binary.**
//!
//! # What this closes
//!
//! Final-Wave report `.omo/evidence/F1-REPORT-wave4.md` records, verbatim:
//!
//! > All 23 upstream commands have one disposition and implemented dispatch arms
//! > are mutation-covered, but only selected command families receive oracle
//! > output comparison. No matrix executes every implemented command and compares
//! > normalized exit status/stdout/stderr.
//!
//! Its blocking finding 1 asks for exactly that, "while retaining the
//! production-dispatch mutation guard". That guard is
//! `tests/surface.rs::surface_every_implemented_command_reaches_its_handler_through_the_production_dispatcher`,
//! and it answers a different question — *did this argv reach its real handler?* —
//! by looking for a fragment only that handler can emit. It is deliberately
//! untouched, because it is the one test that fails when a `match` arm in
//! `cmd/mod.rs` is rerouted to the pending dispatcher, and nothing here would
//! notice that if the oracle happened to fail in a matching way.
//!
//! # Why the table is a bijection with the disposition table
//!
//! [`PARITY_ROWS`] is keyed by the same CLI spelling [`oc_cli::dispositions`] uses,
//! and [`every_implemented_command_has_exactly_one_parity_row`] proves the two are
//! one-to-one in both directions. A thirteenth command becoming
//! [`Disposition::Implemented`] therefore fails this target until it is compared or
//! explicitly exempted — the plan's failure scenario — and a row naming a command
//! that is no longer implemented fails too, so the table cannot keep describing
//! comparisons that stopped meaning anything.
//!
//! # Exemptions are per command, per stream, and visible
//!
//! Two earlier artifacts in this project were rejected for exempting the hard
//! cases until nothing was compared. Three assertions make that impossible to do
//! quietly:
//!
//! * a stream is either compared or carries an [`Stream::Exempt`] reason, and
//!   [`every_exemption_states_a_reason_and_keeps_a_witness`] requires the reason to
//!   be substantive **and** requires a weaker fact — the [`Witness`] — to still be
//!   observed against the two real processes, so an exemption is never total
//!   blindness. For the three failure-message surfaces that weaker fact is now the
//!   two documented texts rather than a shared non-zero exit, and
//!   [`every_declared_diagnostics_surface_carries_a_two_sided_witness`] keeps it
//!   that way;
//! * [`the_comparison_cannot_shrink_into_exemptions`] pins the floor and freezes
//!   the exempt commands **by name**, so one exemption cannot be traded for another
//!   while a count stays put;
//! * [`every_cited_divergence_is_declared`] resolves every divergence an exemption
//!   or a normalization rule leans on against `docs/divergences.toml`.
//!
//! # What "normalized" is allowed to mean
//!
//! Two things, and nothing else. [`mask_literals`] masks the exact directories this
//! run created, one literal per directory, so a subject that wrote to a
//! *neighbouring* path still diverges. Then
//! [`oc_testkit::normalize_cli_stream`] applies five rules whose negative controls
//! live beside them in `crates/oc-testkit/src/cli_normalize.rs`; four correspond to
//! allow-list entries, and [`the_declared_presentation_divergences_are_live`]
//! re-derives each from the two running binaries so a reverted decision fails
//! rather than silently widening what is forgiven.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use oc_cli::{Disposition, dispositions};
use oc_paths::env::accepted_env_name;
use oc_testkit::{
    DivergenceList, Oracle, RunOutcome, ScriptedEnv, Subject, TestkitError, normalize_cli_stream,
};

/// Restated wherever a test skips, so a reader of the output knows what was
/// wanted. Absence is skippable; disagreement is not.
const NO_ORACLE: &str = "no opencode on PATH and no OC_TESTKIT_ORACLE override";

/// The name each side is invoked by, for `oc_testkit::mask_program_name`.
const ORACLE_PROGRAM: &str = "opencode";
/// The name this port is invoked by.
const SUBJECT_PROGRAM: &str = "zuno";

/// The pinned catalogue two `models` probes need, so the roster is a fixture
/// rather than whatever this machine cached — and so the oracle answers without a
/// network call, which this harness has no capability to make.
const MODELS_FIXTURE: &str = "../oc-llm/tests/fixtures/models-dev-pinned.json";
/// One provider, declared inline, so the catalogue resolves deterministically.
const MODELS_CONFIG: &str = r#"{"provider":{"anyapi":{}}}"#;
/// Placeholder resolved to [`MODELS_FIXTURE`]'s absolute path at run time.
const MODELS_FIXTURE_TOKEN: &str = "<MODELS_FIXTURE>";
/// Placeholder resolved to a port this test holds bound.
const BUSY_PORT_TOKEN: &str = "<BUSY_PORT>";

/// The part of this port's failed-bind message that names what upstream omits.
///
/// The address is asserted through [`BUSY_PORT_TOKEN`], so the witness pins *that
/// the address the run actually occupied is in the message* rather than a literal
/// port number. The trailing cause — `Address already in use (os error 98)` — is
/// deliberately **not** pinned: `98` is Linux's `EADDRINUSE` and the strerror text
/// is the platform's, so pinning it would make this witness fail on a healthy macOS
/// or Windows host for a reason that has nothing to do with the divergence. What
/// the entry promises and a user needs is the address, and that is portable.
const SUBJECT_BIND_FAILURE: &str = "could not bind HTTP server to 127.0.0.1:<BUSY_PORT>";

/// A session id no database contains, so a probe reaches the handler's not-found
/// path without needing a fixture.
const ABSENT_SESSION: &str = "ses_probe000000000000000000000a";

/// The allow-list entry covering the four presentation rules.
const PRESENTATION_DIVERGENCE: &str = "plain-cli-presentation";
/// The allow-list entry covering the message text on failure paths.
const DIAGNOSTICS_DIVERGENCE: &str = "diagnostics-name-their-cause";
/// The allow-list entry covering `session list`'s columns and JSON field set.
const SESSION_LIST_DIVERGENCE: &str = "session-list-output-shape";
/// The allow-list entry covering the plan glob outside a repository.
const NON_VCS_PLAN_DIVERGENCE: &str = "non-vcs-plan-glob-is-absolute";

// ---------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------

/// Whether a stream participates in the comparison, and why not when it does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stream {
    /// Compared after normalization.
    Compared,
    /// Excluded, with the reason a reader is owed.
    Exempt(&'static str),
}

impl Stream {
    fn reason(self) -> Option<&'static str> {
        match self {
            Self::Compared => None,
            Self::Exempt(reason) => Some(reason),
        }
    }

    fn is_exempt(self) -> bool {
        matches!(self, Self::Exempt(_))
    }
}

/// The weaker fact still observed about a probe whose streams are not all compared.
///
/// An exemption without one of these would be a hole. Each variant is checked
/// against the two real processes by
/// [`every_exemption_states_a_reason_and_keeps_a_witness`], so it is an
/// observation rather than a promise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Witness {
    /// Nothing weaker is needed: every stream of this probe is compared.
    FullComparison,
    /// The oracle's raw stderr contains this fragment, which is what proves the
    /// argv means something else entirely upstream.
    OracleStderrContains(&'static str),
    /// Both sides refuse, **and** each side's raw stderr carries the text the
    /// [`DIAGNOSTICS_DIVERGENCE`] entry records for it.
    ///
    /// This replaces a weaker tier that asserted only *both sides refuse*. F4's
    /// fifth-wave review named that gap: a witness which observes nothing but a
    /// shared non-zero exit "does not directly assert the documented stderr
    /// texts", so it cannot tell *failing for the reason we declared* apart from
    /// *failing for some other reason*, and this project has already shipped one
    /// test that passed for a reason other than the one it claimed. Its only two
    /// users were exactly the probes of this divergence, so the weaker tier is
    /// gone rather than left reachable by nothing.
    ///
    /// Four facts are observed per probe, because any three of them leave a hole:
    ///
    /// 1. both sides still refuse — the fact the weaker tier carried, kept;
    /// 2. every [`Self::DocumentedDiagnostics::oracle_form`] fragment is in the
    ///    oracle's stderr, so *upstream's* half of the declaration is measured
    ///    rather than remembered;
    /// 3. every [`Self::DocumentedDiagnostics::subject_form`] fragment is in this
    ///    port's stderr, so the diagnostic this divergence exists to keep cannot
    ///    quietly regress into upstream's opaque wording;
    /// 4. **and neither side carries the other's form.** Asserting only (2) would
    ///    pass if this port degraded to the same opaque text; asserting only (3)
    ///    would pass if a future release started naming the cause too — at which
    ///    point the divergence is closed and the entry has to go, which is a
    ///    failure worth having.
    DocumentedDiagnostics {
        /// Fragments the released binary's stderr carries here — the opaque form
        /// on the two surfaces where it names nothing, and its own wording on the
        /// argv refusal.
        oracle_form: &'static [&'static str],
        /// Fragments this port's stderr carries here: the input, the address or
        /// the cause upstream omits. [`BUSY_PORT_TOKEN`] is resolved to the port
        /// the run actually holds, so the address is asserted without a literal
        /// port number.
        subject_form: &'static [&'static str],
    },
}

/// One invocation, and what is compared about it.
struct Probe {
    argv: &'static [&'static str],
    /// Extra environment both sides receive, identically.
    env: &'static [(&'static str, &'static str)],
    exit: Stream,
    stdout: Stream,
    stderr: Stream,
    witness: Witness,
}

impl Probe {
    fn exemptions(&self) -> Vec<&'static str> {
        [self.exit, self.stdout, self.stderr]
            .into_iter()
            .filter_map(Stream::reason)
            .collect()
    }

    fn fully_compared(&self) -> bool {
        !self.exit.is_exempt() && !self.stdout.is_exempt() && !self.stderr.is_exempt()
    }
}

/// One implemented command's parity row.
struct ParityRow {
    /// The CLI spelling, matched against [`oc_cli::dispositions`].
    command: &'static str,
    probes: &'static [Probe],
}

const NO_ENV: &[(&str, &str)] = &[];
const MODELS_ENV: &[(&str, &str)] = &[
    ("OPENCODE_MODELS_PATH", MODELS_FIXTURE_TOKEN),
    ("OPENCODE_CONFIG_CONTENT", MODELS_CONFIG),
];

/// A probe with every stream compared.
const fn compared(
    argv: &'static [&'static str],
    env: &'static [(&'static str, &'static str)],
) -> Probe {
    Probe {
        argv,
        env,
        exit: Stream::Compared,
        stdout: Stream::Compared,
        stderr: Stream::Compared,
        witness: Witness::FullComparison,
    }
}

/// Every [`Disposition::Implemented`] command, and the invocations compared for it.
///
/// Each probe reaches its handler and returns promptly without a network call, a
/// fixture database, or a write outside the run's temporary root — the discipline
/// `tests/surface.rs`'s probes follow, because a probe needing any of those could
/// not be run against the released binary at all.
static PARITY_ROWS: &[ParityRow] = &[
    ParityRow {
        command: "agent",
        probes: &[compared(&["agent", "list"], NO_ENV)],
    },
    ParityRow {
        command: "db",
        probes: &[
            compared(&["db", "--format", "json", "select 1 as probe"], NO_ENV),
            compared(&["db", "--format", "tsv", "select 1 as probe"], NO_ENV),
        ],
    },
    ParityRow {
        command: "debug",
        probes: &[compared(&["debug", "paths"], NO_ENV)],
    },
    ParityRow {
        command: "export",
        probes: &[compared(&["export", ABSENT_SESSION], NO_ENV)],
    },
    ParityRow {
        command: "import",
        probes: &[compared(&["import", "probe.json"], NO_ENV)],
    },
    ParityRow {
        command: "mcp",
        probes: &[compared(&["mcp", "list"], NO_ENV)],
    },
    ParityRow {
        command: "models",
        probes: &[
            compared(&["models"], MODELS_ENV),
            compared(&["models", "anyapi"], MODELS_ENV),
        ],
    },
    ParityRow {
        command: "providers",
        probes: &[compared(&["providers", "list"], NO_ENV)],
    },
    ParityRow {
        command: "run",
        probes: &[
            Probe {
                argv: &["run"],
                env: NO_ENV,
                exit: Stream::Compared,
                stdout: Stream::Compared,
                stderr: Stream::Exempt(
                    "the refusal is the same refusal in different words — upstream says `You must \
                     provide a message or a command`, this port says `a message is required` — and \
                     it falls under the declared `diagnostics-name-their-cause` divergence. The \
                     command's real work is not comparable at all: measured on release 1.18.18 \
                     under this same cleared environment, `run --agent nosuch hi` ANSWERED from \
                     the bundled `opencode/big-pickle` gateway model with no credential present, \
                     so comparing a turn means making a live provider call. `oc-testkit` has no \
                     HTTP client in its dependency graph and \
                     `crates/oc-testkit/tests/no_http_client.rs` keeps it that way; the turn is \
                     compared against recorded traffic in `crates/oc-cli/tests/tool_turn.rs` \
                     instead.",
                ),
                witness: Witness::DocumentedDiagnostics {
                    oracle_form: &["You must provide a message or a command"],
                    subject_form: &["a message is required"],
                },
            },
            Probe {
                argv: &["run", "--model", "bogus/model", "hi"],
                env: NO_ENV,
                exit: Stream::Compared,
                stdout: Stream::Compared,
                stderr: Stream::Exempt(
                    "the third surface the declared `diagnostics-name-their-cause` divergence \
                     names, and the one where the difference is widest. Measured on 1.18.18 under \
                     this cleared environment, upstream answers an unresolvable model with a JSON \
                     `UnknownError` whose whole actionable content is `Unexpected server error. \
                     Check server logs for details.` plus a `ref` that is a fresh random id on \
                     every run — so the two texts are not two renderings of one message, and \
                     upstream's is not even byte-stable against itself. This port names the model \
                     that was asked for, the catalogue state that could not answer it, and the \
                     three ways to fix it. The witness asserts both halves against both processes, \
                     so neither a regression to upstream's opaque form nor upstream adopting ours \
                     can pass as `both still fail`.",
                ),
                witness: Witness::DocumentedDiagnostics {
                    oracle_form: &[
                        "UnknownError",
                        "Unexpected server error. Check server logs for details.",
                    ],
                    subject_form: &[
                        "model `bogus/model` is not available",
                        "Define the provider and model under `provider` in your config",
                        "ZUNO_MODELS_PATH to a catalog file on disk",
                    ],
                },
            },
        ],
    },
    ParityRow {
        command: "serve",
        probes: &[Probe {
            argv: &[
                "serve",
                "--port",
                BUSY_PORT_TOKEN,
                "--hostname",
                "127.0.0.1",
            ],
            env: NO_ENV,
            exit: Stream::Compared,
            stdout: Stream::Compared,
            stderr: Stream::Exempt(
                "upstream reports a failed bind as the opaque two-line `Unexpected error` / \
                 `ServeError`, naming neither the address nor the cause; this port names both — \
                 `could not bind HTTP server to 127.0.0.1:<port>: Address already in use (os error \
                 98)`. That is the declared `diagnostics-name-their-cause` divergence, and this \
                 port's message embeds the address it tried, so the two texts cannot be made equal \
                 without deleting information a user needs. The probe binds an occupied port \
                 because a bare `serve` listens forever: measured on 1.18.18, it never exits, so \
                 there is no output to compare.",
            ),
            witness: Witness::DocumentedDiagnostics {
                oracle_form: &["Unexpected error", "ServeError"],
                subject_form: &[SUBJECT_BIND_FAILURE],
            },
        }],
    },
    ParityRow {
        command: "session",
        probes: &[
            compared(&["session", "list"], NO_ENV),
            compared(&["session", "delete", ABSENT_SESSION], NO_ENV),
        ],
    },
    ParityRow {
        command: "tui",
        probes: &[Probe {
            argv: &["tui"],
            env: NO_ENV,
            exit: Stream::Exempt(
                "`tui` is this port's spelling for the command upstream registers as `$0`, with no \
                 name of its own, so this argv does not ask the two binaries the same question: \
                 upstream reads `tui` as the positional *directory* to work in and fails with \
                 `Failed to change directory to <PROJECT>/tui` while still exiting 0, and this \
                 port routes it to the terminal application, which refuses a piped stdout and \
                 exits 1. No argv asks both binaries to start an interactive session, and an \
                 interactive session has no comparable streams in any case. The terminal behaviour \
                 is covered by `crates/oc-cli/tests/tui_turn.rs` and the lease tests in `oc-tui`.",
            ),
            stdout: Stream::Compared,
            stderr: Stream::Exempt(
                "the same cause: the two binaries were asked different questions, so their answers \
                 are not two renderings of one answer. The witness observes upstream's \
                 change-directory failure, which is what proves the spellings differ rather than \
                 the behaviour.",
            ),
            witness: Witness::OracleStderrContains("Failed to change directory"),
        }],
    },
];

/// The commands permitted to carry any exemption at all, frozen **by name**.
///
/// A count alone could not catch a swap — one command's exemption disappearing as
/// another's appears leaves the number at three — so the members are listed and
/// [`the_comparison_cannot_shrink_into_exemptions`] compares this against what the
/// table actually says.
const COMMANDS_WITH_EXEMPTIONS: &[&str] = &["run", "serve", "tui"];

/// The three surfaces [`DIAGNOSTICS_DIVERGENCE`] declares, spelled as the argv that
/// reaches each, frozen **by name**.
///
/// The entry names `serve` on an unavailable port, `run` with no message, and `run`
/// with an unresolvable model. Two of the three had a probe that observed only a
/// shared non-zero exit and the third had no probe at all, so the declaration
/// outran what was measured.
/// [`every_declared_diagnostics_surface_carries_a_two_sided_witness`] compares this
/// list against the table, and it does so **without running either binary**, so
/// downgrading a probe back to a weaker witness fails even on a host with no oracle
/// installed.
const DIAGNOSTICS_SURFACES: &[&str] = &[
    "run",
    "run --model bogus/model hi",
    "serve --port <BUSY_PORT> --hostname 127.0.0.1",
];

/// How many probes the comparison runs. Pinned because each probe is one process
/// per side, so a shrinking count is how a comparison quietly stops covering what
/// it claims.
const PROBE_COUNT: usize = 16;

// ---------------------------------------------------------------------------
// Running one probe against both binaries
// ---------------------------------------------------------------------------

fn models_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(MODELS_FIXTURE)
}

/// The scripted world one side runs in.
///
/// A real repository is initialized in the project so both binaries classify it the
/// same way. An empty `.git` directory is not enough: upstream resolves the worktree
/// with `git rev-parse --show-toplevel`, which rejects one, and it then reports a
/// *different* plan glob in `agent list`. That difference is real, is declared as
/// [`NON_VCS_PLAN_DIVERGENCE`], and is asserted by
/// [`the_non_vcs_plan_glob_difference_is_live`] rather than smoothed over here.
fn scripted_world(extra: &[(&str, &str)]) -> ScriptedEnv {
    let mut env = ScriptedEnv::new().expect("scripted env");
    initialize_repository(env.project());
    env = env
        .set("NO_COLOR", "1")
        .set("TERM", "dumb")
        .set("OPENCODE_DISABLE_DEFAULT_PLUGINS", "1")
        .set("OPENCODE_DISABLE_LSP_DOWNLOAD", "1");
    for (key, value) in extra {
        let resolved = if *value == MODELS_FIXTURE_TOKEN {
            models_fixture().to_string_lossy().into_owned()
        } else {
            (*value).to_owned()
        };
        env = env.set(*key, resolved);
    }
    env
}

fn subject_world(extra: &[(&str, &str)]) -> ScriptedEnv {
    let env = scripted_world(extra);
    env.env_vars().into_iter().fold(env, |env, (key, value)| {
        let accepted = accepted_env_name(&key).to_owned();
        if accepted == key {
            env
        } else {
            env.set(accepted, value)
        }
    })
}

/// Make `directory` a real repository, failing loudly when `git` is absent.
///
/// A silent fallback would be the worst outcome: the probes would keep running and
/// keep comparing, against a project shape neither binary was asked about.
fn initialize_repository(directory: &Path) {
    let status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(directory)
        .status()
        .unwrap_or_else(|error| {
            panic!(
                "this comparison needs `git` to make {} a repository, because upstream resolves \
                 the worktree with `git rev-parse --show-toplevel` and reports a different plan \
                 glob outside one: {error}",
                directory.display()
            )
        });
    assert!(
        status.success(),
        "`git init` failed in {}; without a repository the two binaries are asked different \
         questions and the comparison would be meaningless",
        directory.display()
    );
}

fn resolved_argv(probe: &Probe, busy_port: u16) -> Vec<String> {
    probe
        .argv
        .iter()
        .map(|argument| {
            if *argument == BUSY_PORT_TOKEN {
                busy_port.to_string()
            } else {
                (*argument).to_owned()
            }
        })
        .collect()
}

/// The exact directory literals one scripted world created, longest first.
///
/// Longest first so a nested directory is masked as itself and not as its parent.
/// Every entry is a string this fixture chose, never a pattern, so a path the
/// subject got wrong cannot be masked by accident.
fn mask_literals(env: &ScriptedEnv) -> Vec<(String, &'static str)> {
    let mut literals: Vec<(String, &'static str)> = vec![
        (display(env.home()), "<HOME>"),
        (display(env.xdg_data()), "<DATA>"),
        (display(env.xdg_config()), "<CONFIG>"),
        (display(env.xdg_cache()), "<CACHE>"),
        (display(env.xdg_state()), "<STATE>"),
        (display(env.tmpdir()), "<TMP>"),
        (display(env.project()), "<PROJECT>"),
        (display(env.root()), "<ROOT>"),
    ];
    literals.sort_by_key(|(literal, _)| std::cmp::Reverse(literal.len()));
    literals
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn apply_masks(text: &str, literals: &[(String, &'static str)]) -> String {
    let mut masked = text.to_owned();
    for (literal, token) in literals {
        if !literal.is_empty() {
            masked = masked.replace(literal.as_str(), token);
        }
    }
    masked
}

/// What one side produced, both raw and normalized.
struct SideOutcome {
    exit: Option<i32>,
    stdout: String,
    stderr: String,
    raw_stdout: String,
    raw_stderr: String,
    rendered: String,
}

fn normalized(
    outcome: &RunOutcome,
    literals: &[(String, &'static str)],
    program: &str,
) -> SideOutcome {
    SideOutcome {
        exit: outcome.exit_code,
        stdout: normalize_cli_stream(&apply_masks(&outcome.stdout, literals), program),
        stderr: normalize_cli_stream(&apply_masks(&outcome.stderr, literals), program),
        raw_stdout: outcome.stdout.clone(),
        raw_stderr: outcome.stderr.clone(),
        rendered: outcome.render(),
    }
}

fn run_oracle(probe: &Probe, busy_port: u16) -> SideOutcome {
    let oracle = Oracle::discover_pinned()
        .expect("the pinned oracle must resolve")
        .with_env(scripted_world(probe.env));
    let outcome = oracle
        .run(resolved_argv(probe, busy_port))
        .expect("the oracle must run");
    normalized(&outcome, &mask_literals(oracle.env()), ORACLE_PROGRAM)
}

fn run_subject(probe: &Probe, busy_port: u16) -> SideOutcome {
    let subject = Subject::at(env!("CARGO_BIN_EXE_zuno"))
        .expect("the shipped binary must exist")
        .with_env(subject_world(probe.env));
    let outcome = subject
        .run(resolved_argv(probe, busy_port))
        .expect("the subject must run");
    normalized(&outcome, &mask_literals(subject.env()), SUBJECT_PROGRAM)
}

/// One probe's verdict against both binaries.
struct ProbeVerdict {
    argv: Vec<String>,
    differences: Vec<String>,
    oracle: SideOutcome,
    subject: SideOutcome,
}

fn compare(probe: &Probe, busy_port: u16) -> ProbeVerdict {
    let mut oracle = run_oracle(probe, busy_port);
    let subject = run_subject(probe, busy_port);
    oracle.stdout = expected_zuno_paths(&oracle.stdout);
    oracle.stderr = expected_zuno_paths(&oracle.stderr);
    let mut differences = Vec::new();

    if probe.exit == Stream::Compared && oracle.exit != subject.exit {
        differences.push(format!(
            "exit status: oracle {:?}, subject {:?}\n{}{}",
            oracle.exit, subject.exit, oracle.rendered, subject.rendered
        ));
    }
    if probe.stdout == Stream::Compared && oracle.stdout != subject.stdout {
        differences.push(format!(
            "stdout:\n--- oracle (normalized) ---\n{}\n--- subject (normalized) ---\n{}",
            oracle.stdout, subject.stdout
        ));
    }
    if probe.stderr == Stream::Compared && oracle.stderr != subject.stderr {
        differences.push(format!(
            "stderr:\n--- oracle (normalized) ---\n{}\n--- subject (normalized) ---\n{}",
            oracle.stderr, subject.stderr
        ));
    }
    ProbeVerdict {
        argv: resolved_argv(probe, busy_port),
        differences,
        oracle,
        subject,
    }
}

fn expected_zuno_paths(text: &str) -> String {
    [
        ("<DATA>/opencode", "<DATA>/zuno"),
        ("<CACHE>/opencode", "<CACHE>/zuno"),
        ("<CONFIG>/opencode", "<CONFIG>/zuno"),
        ("<STATE>/opencode", "<STATE>/zuno"),
        ("<TMP>/opencode", "<TMP>/zuno"),
        ("/opencode/", "/zuno/"),
    ]
    .into_iter()
    .fold(text.to_owned(), |text, (old, new)| text.replace(old, new))
}

/// One [`Witness::DocumentedDiagnostics`] row, checked against both processes.
///
/// Each fragment is checked in **both** directions — present on the side that
/// declares it, absent on the other — because one direction alone is the hole F4
/// found: "both sides fail" is compatible with the two messages having become the
/// same opaque text, and with upstream having started naming the cause.
fn documented_diagnostics_hold(
    command: &str,
    verdict: &ProbeVerdict,
    oracle_form: &[&str],
    subject_form: &[&str],
    busy_port: u16,
) {
    for fragment in oracle_form {
        let fragment = resolve_busy_port(fragment, busy_port);
        assert!(
            verdict.oracle.raw_stderr.contains(&fragment),
            "`{command}`'s exemption rests on release {} answering {:?} with {fragment:?}, which \
             `{DIAGNOSTICS_DIVERGENCE}` records. It emitted:\n{}",
            oc_testkit::PINNED_RELEASE,
            verdict.argv,
            verdict.oracle.raw_stderr
        );
        assert!(
            !verdict.subject.raw_stderr.contains(&fragment),
            "this port answered {:?} with upstream's own {fragment:?}. The exemption exists \
             *because* this port names what that text omits, so carrying it means the declared \
             improvement is gone and the stderr comparison should be restored rather than \
             forgiven. It emitted:\n{}",
            verdict.argv,
            verdict.subject.raw_stderr
        );
    }
    for fragment in subject_form {
        let fragment = resolve_busy_port(fragment, busy_port);
        assert!(
            verdict.subject.raw_stderr.contains(&fragment),
            "`{DIAGNOSTICS_DIVERGENCE}` declares that this port answers {:?} by naming the cause, \
             evidenced by {fragment:?}. `{command}` emitted:\n{}",
            verdict.argv,
            verdict.subject.raw_stderr
        );
        assert!(
            !verdict.oracle.raw_stderr.contains(&fragment),
            "release {} now says {fragment:?} for {:?} too. That closes \
             `{DIAGNOSTICS_DIVERGENCE}` on this surface: delete the exemption, compare the stderr, \
             and remove the entry's clause rather than keeping a declared difference neither \
             binary has. It emitted:\n{}",
            oc_testkit::PINNED_RELEASE,
            verdict.argv,
            verdict.oracle.raw_stderr
        );
    }
}

/// [`BUSY_PORT_TOKEN`] resolved to the port this run holds, so a witness names the
/// address without a literal port number.
fn resolve_busy_port(fragment: &str, busy_port: u16) -> String {
    fragment.replace(BUSY_PORT_TOKEN, &busy_port.to_string())
}

/// A loopback port held bound for the caller's lifetime, so `serve` has something
/// to fail against.
fn hold_a_busy_port() -> (std::net::TcpListener, u16) {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("bind a loopback port to occupy");
    let port = listener.local_addr().expect("local address").port();
    (listener, port)
}

/// `false` when no oracle is installed, so absence skips while disagreement does
/// not: [`Oracle::discover_pinned`] panics on a binary that is a different release.
fn oracle_is_available() -> bool {
    match Oracle::discover() {
        Ok(_) => true,
        Err(TestkitError::BinaryNotFound { .. }) => false,
        Err(other) => {
            panic!("resolving the oracle failed for a reason other than absence: {other}")
        }
    }
}

// ---------------------------------------------------------------------------
// The comparison
// ---------------------------------------------------------------------------

/// **Every implemented command, executed against both binaries and compared on
/// normalized exit status, stdout and stderr.**
///
/// F1's blocking finding 1. Sixteen probes over twelve commands, one process per
/// side each, reporting *every* difference rather than the first so one failure
/// names the whole gap.
#[test]
fn every_implemented_command_produces_the_same_normalized_output_as_the_oracle() {
    if !oracle_is_available() {
        eprintln!(
            "SKIPPED every_implemented_command_produces_the_same_normalized_output_as_the_oracle: \
             {NO_ORACLE}; NO command's output was compared"
        );
        return;
    }
    let (_listener, busy_port) = hold_a_busy_port();
    let mut failures = Vec::new();
    let mut probes_run = 0usize;

    for row in PARITY_ROWS {
        for probe in row.probes {
            let verdict = compare(probe, busy_port);
            probes_run += 1;
            if !verdict.differences.is_empty() {
                failures.push(format!(
                    "`{}` — {:?} diverged in {} stream(s):\n{}",
                    row.command,
                    verdict.argv,
                    verdict.differences.len(),
                    verdict.differences.join("\n")
                ));
            }
            eprintln!(
                "parity {:<10} {:<48} exit o={:?} s={:?}  stdout={:<19} stderr={}",
                row.command,
                verdict.argv.join(" "),
                verdict.oracle.exit,
                verdict.subject.exit,
                stream_label(
                    probe.stdout,
                    verdict.oracle.stdout == verdict.subject.stdout
                ),
                stream_label(
                    probe.stderr,
                    verdict.oracle.stderr == verdict.subject.stderr
                ),
            );
        }
    }
    assert_eq!(
        probes_run, PROBE_COUNT,
        "the probe count changed; move PROBE_COUNT in the same commit and say why"
    );
    assert!(
        failures.is_empty(),
        "{} of {probes_run} probe(s) diverged from release {}:\n\n{}",
        failures.len(),
        oc_testkit::PINNED_RELEASE,
        failures.join("\n\n")
    );
}

fn stream_label(stream: Stream, equal: bool) -> &'static str {
    match (stream, equal) {
        (Stream::Compared, true) => "same",
        (Stream::Compared, false) => "DIFFERENT",
        (Stream::Exempt(_), true) => "exempt(same anyway)",
        (Stream::Exempt(_), false) => "exempt",
    }
}

// ---------------------------------------------------------------------------
// The table cannot be narrowed by omission
// ---------------------------------------------------------------------------

/// **A new implemented command that joins no parity row fails here.**
///
/// The plan's failure scenario, in both directions: an implemented command with no
/// row is uncompared, and a row for a command that is no longer implemented is a
/// claim about something that does not exist.
#[test]
fn every_implemented_command_has_exactly_one_parity_row() {
    let implemented: BTreeSet<&str> = dispositions()
        .iter()
        .filter(|entry| entry.disposition == Disposition::Implemented)
        .map(|entry| entry.command)
        .collect();
    let rows: BTreeSet<&str> = PARITY_ROWS.iter().map(|row| row.command).collect();

    let missing: Vec<&&str> = implemented.difference(&rows).collect();
    assert!(
        missing.is_empty(),
        "{missing:?} became `Disposition::Implemented` without joining PARITY_ROWS, so nothing \
         compares their output against the oracle. Add a row: compare every stream, or exempt a \
         stream with a reason and a witness."
    );
    let stale: Vec<&&str> = rows.difference(&implemented).collect();
    assert!(
        stale.is_empty(),
        "PARITY_ROWS names {stale:?}, which are not implemented dispositions; a row for an \
         unimplemented command claims a comparison nothing performs"
    );
    assert_eq!(
        PARITY_ROWS.len(),
        rows.len(),
        "PARITY_ROWS contains a duplicate command"
    );
    assert_eq!(
        implemented.len(),
        12,
        "the implemented command count changed; F1's finding is about *all* of them, so this \
         number and the table move together"
    );
    let probes: usize = PARITY_ROWS.iter().map(|row| row.probes.len()).sum();
    assert_eq!(
        probes, PROBE_COUNT,
        "PROBE_COUNT must equal what the table declares"
    );
    for row in PARITY_ROWS {
        assert!(
            !row.probes.is_empty(),
            "`{}` has a parity row with no probe, which compares nothing",
            row.command
        );
    }
}

/// Every exemption is named, substantive, and leaves a live weaker observation.
#[test]
fn every_exemption_states_a_reason_and_keeps_a_witness() {
    let oracle_present = oracle_is_available();
    let (_listener, busy_port) = hold_a_busy_port();
    let mut exempt_streams = 0usize;
    let mut witnesses_observed = 0usize;
    let mut diagnostics_witnesses_observed = 0usize;

    for row in PARITY_ROWS {
        for probe in row.probes {
            let reasons = probe.exemptions();
            for reason in &reasons {
                exempt_streams += 1;
                assert!(
                    reason.len() > 120,
                    "`{}`'s exemption is too short to be a reason: {reason:?}. An exemption a \
                     reader cannot evaluate is the invisible kind two earlier reviews rejected.",
                    row.command
                );
            }
            if reasons.is_empty() {
                assert_eq!(
                    probe.witness,
                    Witness::FullComparison,
                    "`{}` compares every stream and must not claim a weaker witness",
                    row.command
                );
                continue;
            }
            assert_ne!(
                probe.witness,
                Witness::FullComparison,
                "`{}` exempts a stream and so must name the weaker fact still observed",
                row.command
            );
            assert!(
                probe.exit == Stream::Compared || probe.stdout == Stream::Compared,
                "`{}` exempts both its exit status and its stdout; at least one observable must \
                 stay compared or the row proves nothing",
                row.command
            );
            if !oracle_present {
                continue;
            }
            let verdict = compare(probe, busy_port);
            witnesses_observed += 1;
            match probe.witness {
                Witness::FullComparison => unreachable!("handled above"),
                Witness::DocumentedDiagnostics {
                    oracle_form,
                    subject_form,
                } => {
                    assert!(
                        verdict.oracle.exit != Some(0) && verdict.subject.exit != Some(0),
                        "`{}`'s witness claims both sides refuse {:?}, but they exited oracle \
                         {:?} / subject {:?}",
                        row.command,
                        verdict.argv,
                        verdict.oracle.exit,
                        verdict.subject.exit
                    );
                    documented_diagnostics_hold(
                        row.command,
                        &verdict,
                        oracle_form,
                        subject_form,
                        busy_port,
                    );
                    diagnostics_witnesses_observed += 1;
                }
                Witness::OracleStderrContains(fragment) => assert!(
                    verdict.oracle.raw_stderr.contains(fragment),
                    "`{}`'s exemption rests on the oracle reading {:?} differently, evidenced by \
                     {fragment:?} in its stderr. It emitted:\n{}",
                    row.command,
                    verdict.argv,
                    verdict.oracle.raw_stderr
                ),
            }
        }
    }
    assert!(
        exempt_streams >= 4,
        "only {exempt_streams} exempt stream(s) found; the witness machinery would be unexercised. \
         If every stream became comparable, delete the Exempt variant rather than leaving a check \
         nothing reaches."
    );
    if oracle_present {
        assert_eq!(
            witnesses_observed, 4,
            "every exempting probe must have had its witness observed against both binaries"
        );
        assert_eq!(
            diagnostics_witnesses_observed,
            DIAGNOSTICS_SURFACES.len(),
            "`{DIAGNOSTICS_DIVERGENCE}` names {} surfaces and each one's documented texts must \
             have been observed against both binaries; a surface losing its two-sided witness is \
             how this entry drifted back to `both still fail` once already",
            DIAGNOSTICS_SURFACES.len()
        );
    } else {
        eprintln!(
            "SKIPPED the witness half of every_exemption_states_a_reason_and_keeps_a_witness: \
             {NO_ORACLE}; the reasons were checked, the witnesses were NOT"
        );
    }
}

/// **The comparison cannot shrink into exemptions.**
///
/// Fixes a floor on how much is actually compared, and freezes the exempt commands
/// by name so one exemption cannot be traded for another.
#[test]
fn the_comparison_cannot_shrink_into_exemptions() {
    let exempt: BTreeSet<&str> = PARITY_ROWS
        .iter()
        .filter(|row| row.probes.iter().any(|probe| !probe.fully_compared()))
        .map(|row| row.command)
        .collect();
    let frozen: BTreeSet<&str> = COMMANDS_WITH_EXEMPTIONS.iter().copied().collect();
    assert_eq!(
        exempt, frozen,
        "the set of commands carrying an exemption changed. Every member is justified in its row \
         and named in COMMANDS_WITH_EXEMPTIONS; a command leaving the set is progress and must be \
         recorded, and a command joining it is a narrowing that has to be reviewed."
    );

    let fully_compared = PARITY_ROWS
        .iter()
        .filter(|row| row.probes.iter().all(Probe::fully_compared))
        .count();
    assert!(
        fully_compared >= 9,
        "only {fully_compared} of {} commands have every stream compared; the floor is nine, \
         because a table that exempts its way to green is what two earlier reviews rejected",
        PARITY_ROWS.len()
    );

    let exit_and_stdout = PARITY_ROWS
        .iter()
        .filter(|row| {
            row.probes
                .iter()
                .all(|probe| probe.exit == Stream::Compared && probe.stdout == Stream::Compared)
        })
        .count();
    assert!(
        exit_and_stdout >= 11,
        "only {exit_and_stdout} of {} commands have both exit status and stdout compared; the \
         floor is eleven",
        PARITY_ROWS.len()
    );
}

/// Every divergence this target's normalization or exemptions lean on is declared.
///
/// A normalization whose justification is not in the allow-list is exactly the
/// laundering the plan forbids, so the ids are resolved against the loaded file
/// rather than being written down twice.
#[test]
fn every_cited_divergence_is_declared() {
    let list = DivergenceList::load().expect("docs/divergences.toml must load");
    for id in [
        PRESENTATION_DIVERGENCE,
        DIAGNOSTICS_DIVERGENCE,
        SESSION_LIST_DIVERGENCE,
        NON_VCS_PLAN_DIVERGENCE,
    ] {
        let entry = list.find(id).unwrap_or_else(|| {
            panic!(
                "this target's normalization and exemptions rest on divergence {id:?}, which {} \
                 does not declare",
                list.path().display()
            )
        });
        assert!(
            !entry.reason.trim().is_empty(),
            "divergence {id:?} carries no reason"
        );
    }
    for row in PARITY_ROWS {
        for reason in row.probes.iter().flat_map(Probe::exemptions) {
            for cited in [DIAGNOSTICS_DIVERGENCE, PRESENTATION_DIVERGENCE] {
                if reason.contains(cited) {
                    assert!(
                        list.find(cited).is_some(),
                        "`{}` cites {cited:?}, which is not declared",
                        row.command
                    );
                }
            }
        }
    }
    for rule in oc_testkit::CLI_RULE_NAMES {
        assert!(
            !rule.is_empty(),
            "every normalization rule must be named so the report can print it"
        );
    }
    assert_eq!(
        oc_testkit::CLI_RULE_NAMES.len(),
        5,
        "the CLI normalization rule set changed; a new rule widens what two binaries may disagree \
         about and needs an allow-list entry and a liveness assertion"
    );
}

/// **Every surface `diagnostics-name-their-cause` declares carries a witness that
/// names both texts, and the declaration quotes what that witness pins.**
///
/// The gap F4's fifth-wave review found: this entry's probes asserted only that
/// both sides refuse, which "does not directly assert the documented stderr texts"
/// the way the other three of todo 135's divergences do. A shared non-zero exit
/// cannot tell *failing for the declared reason* apart from *failing for some other
/// reason* — the same defect as a permission test that once passed because a scope
/// did not match rather than because a ticket had expired.
///
/// Two things are checked here, and neither needs a process, so both hold on a host
/// with no oracle:
///
/// * the table's two-sided witnesses cover exactly [`DIAGNOSTICS_SURFACES`], so a
///   probe cannot be downgraded to a weaker tier or dropped;
/// * the declared reason quotes every fragment the witnesses expect **of upstream**,
///   because that half is the one no code here controls: a release can change its
///   own wording, and when it does, the entry a reader trusts must be the thing that
///   fails. This port's half is pinned against the running binary instead, which is
///   stronger than prose, and the address form is checked here too since it is the
///   example the entry leads with.
#[test]
fn every_declared_diagnostics_surface_carries_a_two_sided_witness() {
    let mut witnessed: BTreeSet<String> = BTreeSet::new();
    let mut oracle_fragments: BTreeSet<&str> = BTreeSet::new();
    for row in PARITY_ROWS {
        for probe in row.probes {
            if let Witness::DocumentedDiagnostics {
                oracle_form,
                subject_form,
            } = probe.witness
            {
                assert!(
                    probe.stderr.is_exempt(),
                    "`{}` claims the documented-diagnostics witness for {:?}, but its stderr is \
                     compared; the two-sided witness exists to replace a comparison, not to sit \
                     beside one",
                    row.command,
                    probe.argv
                );
                assert!(
                    !oracle_form.is_empty() && !subject_form.is_empty(),
                    "`{}`'s witness for {:?} must name a fragment for each side; an empty list \
                     asserts nothing while looking like the stronger tier",
                    row.command,
                    probe.argv
                );
                witnessed.insert(probe.argv.join(" "));
                oracle_fragments.extend(oracle_form.iter().copied());
            }
        }
    }
    let declared: BTreeSet<String> = DIAGNOSTICS_SURFACES
        .iter()
        .map(|surface| (*surface).to_owned())
        .collect();
    assert_eq!(
        witnessed, declared,
        "the set of argvs carrying a two-sided `{DIAGNOSTICS_DIVERGENCE}` witness changed. Every \
         surface the entry declares must have one: a surface losing it reverts to the `both still \
         fail` witness F4 rejected, and a surface gaining one that the entry does not declare is \
         an exemption resting on nothing."
    );

    let list = DivergenceList::load().expect("docs/divergences.toml must load");
    let entry = list
        .find(DIAGNOSTICS_DIVERGENCE)
        .unwrap_or_else(|| panic!("{DIAGNOSTICS_DIVERGENCE} must be declared"));
    for fragment in &oracle_fragments {
        assert!(
            entry.reason.contains(fragment),
            "the witnesses expect release {} to say {fragment:?}, but the declared reason never \
             quotes it. A reader is then told one thing and the test enforces another, which is \
             exactly the prose-versus-executable drift the fifth-wave review blocked elsewhere.",
            oc_testkit::PINNED_RELEASE
        );
    }
    let named_address = SUBJECT_BIND_FAILURE
        .split(BUSY_PORT_TOKEN)
        .next()
        .expect("the bind fragment must have a literal prefix");
    assert!(
        entry.reason.contains(named_address),
        "the entry must quote this port's {named_address:?} form, which is the improvement it \
         exists to protect"
    );
}

// ---------------------------------------------------------------------------
// The declared differences are re-derived from the binaries, not asserted
// ---------------------------------------------------------------------------

/// **Each declared presentation divergence is still live.**
///
/// A declared difference whose behaviour silently reverted is worse than an
/// undeclared one: the allow-list would then state, with a reason, something
/// neither binary does, and the matching normalization rule would be free to hide a
/// real difference. So each is re-derived from the two running processes.
#[test]
fn the_declared_presentation_divergences_are_live() {
    if !oracle_is_available() {
        eprintln!(
            "SKIPPED the_declared_presentation_divergences_are_live: {NO_ORACLE}; the declared \
             presentation divergences were NOT verified"
        );
        return;
    }
    let (_listener, busy_port) = hold_a_busy_port();

    let verdict = compare(&compared(&["import", "probe.json"], NO_ENV), busy_port);
    assert!(
        verdict.oracle.raw_stderr.contains('\u{1b}'),
        "`{PRESENTATION_DIVERGENCE}` declares that the released binary emits SGR colour even under \
         NO_COLOR=1 and TERM=dumb. It emitted none: {:?}. If upstream started honouring NO_COLOR, \
         delete the `sgr-colour` rule and this part of the declaration together.",
        verdict.oracle.raw_stderr
    );
    assert!(
        !verdict.subject.raw_stderr.contains('\u{1b}'),
        "this port must emit no colour under NO_COLOR=1: {:?}",
        verdict.subject.raw_stderr
    );
    assert!(
        oc_testkit::strip_sgr(&verdict.oracle.raw_stderr).starts_with("Error: "),
        "`{PRESENTATION_DIVERGENCE}` declares upstream's line-leading `Error: ` prefix. Upstream \
         said: {:?}",
        verdict.oracle.raw_stderr
    );
    assert!(
        !verdict.subject.raw_stderr.starts_with("Error: "),
        "this port must not print the prefix the normalization strips: {:?}",
        verdict.subject.raw_stderr
    );

    let verdict = compare(&compared(&["mcp", "list"], NO_ENV), busy_port);
    assert!(
        verdict.oracle.raw_stdout.contains('\u{250c}')
            && verdict.oracle.raw_stdout.contains('\u{2514}'),
        "`{PRESENTATION_DIVERGENCE}` declares upstream's @clack/prompts box gutter on `mcp list`. \
         It printed: {:?}",
        verdict.oracle.raw_stdout
    );
    assert!(
        !verdict.subject.raw_stdout.contains('\u{250c}'),
        "this port must print plain lines: {:?}",
        verdict.subject.raw_stdout
    );

    let verdict = compare(&compared(&["agent", "list"], NO_ENV), busy_port);
    assert_ne!(
        verdict.oracle.raw_stdout, verdict.subject.raw_stdout,
        "the `json-key-order` rule exists because these two agree only once their JSON object keys \
         are sorted. If they now agree byte for byte, delete the rule."
    );
    assert_eq!(
        verdict.oracle.stdout, verdict.subject.stdout,
        "…and once sorted they must agree"
    );
}

/// **The plan glob outside a repository is a real difference, declared and
/// measured.**
///
/// Every parity probe runs in a directory marked as a worktree, which is a choice
/// this file has to justify: in a directory that is *not* a repository the two
/// binaries disagree. Upstream's `path.relative(ctx.worktree, …)` is computed
/// against the worktree `/` it assigns a non-VCS project
/// (`packages/opencode/src/project/project.ts:217`), producing a **relative** glob
/// with no leading separator; this port emits the absolute path. This test runs
/// both binaries in an unmarked directory so the declaration carries a measurement.
#[test]
fn the_non_vcs_plan_glob_difference_is_live() {
    if !oracle_is_available() {
        eprintln!(
            "SKIPPED the_non_vcs_plan_glob_difference_is_live: {NO_ORACLE}; the declared \
             `{NON_VCS_PLAN_DIVERGENCE}` divergence was NOT verified"
        );
        return;
    }
    let list = DivergenceList::load().expect("docs/divergences.toml must load");
    assert!(
        list.find(NON_VCS_PLAN_DIVERGENCE).is_some(),
        "{NON_VCS_PLAN_DIVERGENCE} must be declared"
    );

    let oracle_env = ScriptedEnv::new().expect("scripted env");
    let oracle = Oracle::discover_pinned()
        .expect("pinned oracle")
        .with_env(oracle_env.set("NO_COLOR", "1").set("TERM", "dumb"));
    let oracle_out = oracle.run(["agent", "list"]).expect("oracle agent list");

    let subject_env = ScriptedEnv::new().expect("scripted env");
    let subject = Subject::at(env!("CARGO_BIN_EXE_zuno"))
        .expect("shipped binary")
        .with_env(subject_env.set("NO_COLOR", "1").set("TERM", "dumb"));
    let subject_out = subject.run(["agent", "list"]).expect("subject agent list");

    let oracle_glob = plan_edit_glob(&oracle_out.stdout);
    let subject_glob = plan_edit_glob(&subject_out.stdout);
    assert!(
        !oracle_glob.starts_with('/'),
        "`{NON_VCS_PLAN_DIVERGENCE}` declares upstream's glob is relative outside a repository, \
         because it is computed against the worktree `/`. It emitted {oracle_glob:?} from:\n{}",
        oracle_out.render()
    );
    assert!(
        subject_glob.starts_with('/'),
        "the entry declares this port emits an absolute glob there; it emitted {subject_glob:?}"
    );
    assert_ne!(
        oracle_glob, subject_glob,
        "if the two globs now agree the divergence is closed and the entry must be removed"
    );
    eprintln!("non-vcs-plan-glob: oracle {oracle_glob:?}; subject {subject_glob:?}");
}

/// The `plans/*.md` pattern out of an `agent list` dump.
fn plan_edit_glob(stdout: &str) -> String {
    stdout
        .lines()
        .map(str::trim)
        .find(|line| line.contains("plans/*.md") && !line.contains(".opencode/plans"))
        .and_then(|line| line.split('"').nth(3).map(str::to_owned))
        .unwrap_or_else(|| panic!("no plan glob in agent list output:\n{stdout}"))
}

/// **`session list`'s output shape is a real difference, declared and measured.**
///
/// The *empty* listing agrees on both sides, which is what the parity row compares.
/// A non-empty one does not: upstream prints three columns and a six-field JSON
/// object, this port prints seven columns and a nested object using different key
/// names. That is not presentation, and no rule normalizes it — it is declared as
/// [`SESSION_LIST_DIVERGENCE`], and this test re-derives it from one database both
/// binaries open, so the declaration carries a measurement.
///
/// Both binaries run against one shared root here, unlike the parity probes: the
/// listing is project-scoped, so a session seeded under one root's project is
/// invisible from another's. The sequence is oracle-then-subject, and only the JSON
/// field names are read, so the shared root's cross-contamination cannot affect the
/// result.
#[test]
fn the_session_list_output_shape_difference_is_live() {
    if !oracle_is_available() {
        eprintln!(
            "SKIPPED the_session_list_output_shape_difference_is_live: {NO_ORACLE}; the declared \
             `{SESSION_LIST_DIVERGENCE}` divergence was NOT verified"
        );
        return;
    }
    let list = DivergenceList::load().expect("docs/divergences.toml must load");
    let entry = list
        .find(SESSION_LIST_DIVERGENCE)
        .unwrap_or_else(|| panic!("{SESSION_LIST_DIVERGENCE} must be declared"));
    let oracle_program = Oracle::discover_pinned()
        .expect("pinned oracle")
        .program()
        .to_path_buf();

    let root = tempfile::tempdir().expect("shared root");
    let project = root.path().join("project");
    std::fs::create_dir_all(&project).expect("create the shared project");
    initialize_repository(&project);
    let database = root.path().join("shared.db");

    let empty = shared_run(
        &oracle_program,
        &["session", "list"],
        root.path(),
        &database,
        false,
    );
    assert!(
        empty.status.success(),
        "the oracle must create its schema: {}",
        String::from_utf8_lossy(&empty.stderr)
    );
    let projects = shared_run(
        &oracle_program,
        &["db", "--format", "tsv", "select id from project"],
        root.path(),
        &database,
        false,
    );
    let project_id = String::from_utf8_lossy(&projects.stdout)
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty() && *line != "id")
        .unwrap_or_else(|| {
            panic!(
                "the oracle must have created a project row: {}",
                String::from_utf8_lossy(&projects.stdout)
            )
        })
        .to_owned();

    let insert = format!(
        "insert into session (id, project_id, slug, directory, title, version, cost, tokens_input, \
         tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write, time_created, \
         time_updated) values ('{ABSENT_SESSION}', '{project_id}', 'parity', '{}', 'Parity \
         fixture session', '{}', 0, 0, 0, 0, 0, 0, 1770000000000, 1770000000000)",
        project.display(),
        oc_testkit::PINNED_RELEASE
    );
    let seeded = shared_run(
        &oracle_program,
        &["db", &insert],
        root.path(),
        &database,
        false,
    );
    assert!(
        seeded.status.success(),
        "seeding one session must succeed: {}",
        String::from_utf8_lossy(&seeded.stderr)
    );

    let oracle_json = shared_run(
        &oracle_program,
        &["session", "list", "--format", "json"],
        root.path(),
        &database,
        false,
    );
    let subject_json = shared_run(
        Path::new(env!("CARGO_BIN_EXE_zuno")),
        &["session", "list", "--format", "json"],
        root.path(),
        &database,
        true,
    );
    let oracle_keys = json_field_names(&String::from_utf8_lossy(&oracle_json.stdout));
    let subject_keys = json_field_names(&String::from_utf8_lossy(&subject_json.stdout));

    assert!(
        oracle_keys.contains("projectId"),
        "`{SESSION_LIST_DIVERGENCE}` declares upstream's `projectId` spelling; upstream emitted \
         {oracle_keys:?} from:\n{}\n{}",
        String::from_utf8_lossy(&oracle_json.stdout),
        String::from_utf8_lossy(&oracle_json.stderr)
    );
    assert!(
        subject_keys.contains("projectID"),
        "the entry declares this port's `projectID` spelling; it emitted {subject_keys:?} from:\n{}\n{}",
        String::from_utf8_lossy(&subject_json.stdout),
        String::from_utf8_lossy(&subject_json.stderr)
    );
    assert_ne!(
        oracle_keys, subject_keys,
        "`{SESSION_LIST_DIVERGENCE}` declares two different field sets. If they now agree the \
         divergence is closed and the entry must be removed."
    );
    for spelling in ["projectId", "projectID"] {
        assert!(
            entry.reason.contains(spelling),
            "the declared reason must name the {spelling:?} spelling it rests on"
        );
    }
    eprintln!(
        "session-list-output-shape: oracle fields {oracle_keys:?}; subject fields {subject_keys:?}"
    );
}

/// Run one binary against a shared, isolated root.
///
/// Only [`the_session_list_output_shape_difference_is_live`] uses this, because it
/// is the one comparison that needs both binaries to resolve the *same* project.
fn shared_run(
    binary: &Path,
    argv: &[&str],
    root: &Path,
    database: &Path,
    zuno_env: bool,
) -> Output {
    let mut command = Command::new(binary);
    command
        .args(argv)
        .current_dir(root.join("project"))
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("HOME", root.join("home"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("TMPDIR", root.join("tmp"));
    if zuno_env {
        command
            .env("ZUNO_DB", database)
            .env("ZUNO_DISABLE_AUTOUPDATE", "1")
            .env("ZUNO_DISABLE_MODELS_FETCH", "1")
            .env("ZUNO_DISABLE_DEFAULT_PLUGINS", "1")
            .env("ZUNO_DISABLE_LSP_DOWNLOAD", "1");
    } else {
        command
            .env("OPENCODE_DB", database)
            .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
            .env("OPENCODE_DISABLE_MODELS_FETCH", "1")
            .env("OPENCODE_DISABLE_DEFAULT_PLUGINS", "1")
            .env("OPENCODE_DISABLE_LSP_DOWNLOAD", "1");
    }
    command
        .output()
        .unwrap_or_else(|error| panic!("{argv:?} must run: {error}"))
}

/// The distinct object key names in a JSON document.
fn json_field_names(text: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim()) {
        collect_field_names(&value, &mut names);
    }
    names
}

fn collect_field_names(value: &serde_json::Value, into: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, nested) in map {
                into.insert(key.clone());
                collect_field_names(nested, into);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_field_names(item, into);
            }
        }
        _ => {}
    }
}

/// The path masks are exact literals, longest first, never patterns.
///
/// Proven against a real [`ScriptedEnv`] so the literals are the ones a run would
/// actually mask, and against a neighbouring directory the run did not create,
/// which must still be compared.
#[test]
fn root_masking_is_literal_and_longest_first() {
    let env = ScriptedEnv::new().expect("scripted env");
    let literals = mask_literals(&env);
    let lengths: Vec<usize> = literals.iter().map(|(literal, _)| literal.len()).collect();
    assert!(
        lengths.windows(2).all(|pair| pair[0] >= pair[1]),
        "the masks must be longest-first or a nested directory is masked as its parent: {lengths:?}"
    );

    let text = format!(
        "data at {}/opencode and project at {}",
        display(env.xdg_data()),
        display(env.project())
    );
    assert_eq!(
        apply_masks(&text, &literals),
        "data at <DATA>/opencode and project at <PROJECT>"
    );

    let neighbour = format!("{}-elsewhere/x", display(env.project()));
    assert_eq!(
        apply_masks(&neighbour, &literals),
        format!("<PROJECT>-elsewhere/x"),
        "a neighbouring directory shares the prefix, so masking is a prefix replacement and the \
         suffix still diverges"
    );
    assert!(
        !apply_masks("/tmp/oc-testkit-somewhere-else/x", &literals).contains('<'),
        "a temporary directory this run did not create must survive unmasked"
    );

    let map: BTreeMap<String, String> = env.env_vars();
    assert_eq!(
        map.get("OPENCODE_DISABLE_AUTOUPDATE").map(String::as_str),
        Some("1"),
        "the scripted world must keep the no-live-call guarantees the parity probes rely on"
    );
}

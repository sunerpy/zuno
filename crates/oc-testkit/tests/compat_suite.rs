//! The full differential compatibility suite: one command, one report.
//!
//! # What this target is, and what it deliberately is not
//!
//! Ninety-five tasks each built a differential comparison against the real
//! `opencode`: the SQLite schema, the `/api` operation set, the CLI per command,
//! the merged config across fourteen trees, the resolved tool set per model, the
//! search engine, LSP diagnostics, agents, skills, commands, models, paths. Each
//! lives in its own crate and each is green.
//!
//! What none of them can answer is the question a drop-in-replacement claim
//! actually rests on: **what, in total, was proven?** A suite that quietly stopped
//! comparing a surface is green in precisely the same way as one that compares
//! everything. So this target does three things the individual differentials
//! cannot:
//!
//! 1. It **re-asserts the two load-bearing DB contracts itself** — the schema
//!    against a database the real binary created, and the migration-journal
//!    round-trip. These are re-expressed here rather than delegated because the
//!    suite has to be the thing that fails: renaming one index must make
//!    `cargo test --test compat_suite` fail *and name that index*.
//! 2. It **holds a registry of every differential surface**, verifies each named
//!    evidence test still exists in the tree, and records a verdict for it. A
//!    renamed or deleted differential fails here instead of silently shrinking the
//!    claim.
//! 3. It **loads `docs/divergences.toml`** and asserts the count and shape, and
//!    checks the one entry the plan requires to be *verified* rather than merely
//!    declared — the `execute` tool's live parameter schema.
//!
//! It is **not** a re-run of the other crates' tests. Spawning nested `cargo test`
//! would double the workspace's runtime to re-derive results the caller already
//! has, and it would make this target's failure mode "some other target failed",
//! which is exactly the diagnostic that made assembling this necessary.
//!
//! # Why the report is an artifact and not just stdout
//!
//! [`oc_testkit::CompatReport`] is written to disk so plan todos F1-F4 can read
//! what was and was not compared without re-running anything, and so two runs can
//! be diffed. Every surface carries its verdict; every normalization carries its
//! reason; every gap is named as a gap rather than dressed up as a decision.
//!
//! # The skip contract
//!
//! Anything needing the real binary is gated on its presence and **prints an
//! explicit skip**, and the report records [`Verdict::Skipped`] for it. A silent
//! skip that reports green is the failure mode this whole suite exists to prevent.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead as _, BufReader};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener as StdTcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, OnceLock};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use oc_testkit::compat_report::{DivergenceSummary, OracleAvailability, OracleKind};
use oc_testkit::{
    BehaviouralDifference, ComparedSurface, CompatReport, DivergenceList, KnownGap, Normalization,
    Verdict, compat_report, compat_report::SCHEMA_VERSION, divergence,
};

/// The installed release the whole port is measured against.
///
/// Re-exported rather than restated so the report, the DB round-trips and the pin
/// gate cannot name three different builds. The *path* is discovered by
/// [`Oracle::discover_pinned`]; only the release is declared. Before plan todo 130
/// this file hard-coded `…/mise/installs/opencode/1.18.12/opencode` while recording
/// `1.18.13`, and nothing could fail over the difference.
const PINNED_RELEASE: &str = oc_testkit::PINNED_RELEASE;

/// The committed capture of the real binary's OpenAPI document.
///
/// Recaptured from [`PINNED_RELEASE`] for todo 130 by serving `/doc` under an
/// isolated XDG world. The bytes are **identical** to the 1.18.12 capture the name
/// records — 1.18.12, 1.18.13, 1.18.14 and 1.18.15 all emit the same 478,747-byte
/// document, sha256 `c3a9f94af0c3324d97b482b14c692e810ce7ccac3136319ba46334de972b4cf1`
/// — so the filename is the capture's provenance, not a claim that the document is
/// version-specific. That equality is not taken on trust:
/// [`the_committed_openapi_capture_is_what_the_pinned_release_serves`] refetches
/// `/doc` from the running release and compares.
const ORACLE_OPENAPI_FIXTURE: &str = ".omo/fixtures/oracle-openapi-1.18.12.json";

/// Upstream `/api` operations the capture declares. Unchanged from the 1.18.12
/// capture, because the document is byte-identical across all four installed
/// releases.
const UPSTREAM_API_OPERATIONS: usize = 58;

// ---------------------------------------------------------------------------
// Surface registry
// ---------------------------------------------------------------------------

/// One row of the surface registry.
struct SurfaceRow {
    id: &'static str,
    name: &'static str,
    verdict: Verdict,
    oracle: OracleKind,
    /// `crates/<crate>/tests/<file>.rs::<test fn>`, or `(this target)` for the
    /// comparisons the suite performs itself.
    evidence: &'static str,
    detail: &'static str,
}

/// Every surface the port claims to have compared, and every one it has not.
///
/// This table is the report's spine. It is hand-maintained on purpose: the whole
/// point is that a human has to state, for each surface, what was established.
/// [`every_registered_evidence_test_still_exists`] then proves each claim points
/// at a test that exists, so the table cannot describe comparisons that were
/// deleted or renamed away.
const SURFACES: &[SurfaceRow] = &[
    SurfaceRow {
        id: "db-schema",
        name: "SQLite schema: tables, indexes, columns, foreign keys",
        verdict: Verdict::Compared,
        oracle: OracleKind::LiveBinary,
        evidence: "crates/oc-testkit/tests/compat_suite.rs::db_schema_matches_a_database_the_real_binary_created",
        detail: "re-asserted by this target so a renamed index fails here; also covered by crates/oc-db/tests/schema.rs::schema_matches_a_database_created_by_the_real_opencode_binary",
    },
    SurfaceRow {
        id: "db-migration-journal",
        name: "migration journal round-trip through the real binary",
        verdict: Verdict::Compared,
        oracle: OracleKind::LiveBinary,
        evidence: "crates/oc-testkit/tests/compat_suite.rs::journal_round_trip_through_the_real_binary_does_not_replay_migrations",
        detail: "the headline case: a Rust-created database is opened by the real binary, which must not die and must leave the 38 completed ids unchanged",
    },
    SurfaceRow {
        id: "db-session-decode",
        name: "a session row this port wrote, decoded by the real binary",
        verdict: Verdict::Compared,
        oracle: OracleKind::LiveBinary,
        evidence: "crates/oc-testkit/tests/compat_suite.rs::a_session_written_by_this_port_is_decodable_by_the_real_binary",
        detail: "the journal round-trip above proves the real binary survives opening a Rust database; it does not prove the binary can read the rows a Rust turn writes. This one hands it a session written through oc_db::session::create and requires `session list` to exit 0 — the gap that let a `modelID`-spelled session.model reach a release. End-to-end through the production binary in crates/oc-cli/tests/rollback.rs::the_released_binary_lists_a_session_this_port_wrote",
    },
    SurfaceRow {
        id: "api-operations",
        name: "/api per-operation status, normalized body, and side-effect matrix",
        verdict: Verdict::PartiallyCompared,
        oracle: OracleKind::LiveBinary,
        evidence: "crates/oc-testkit/tests/compat_suite.rs::api_behaviour_matrix_compares_live_status_body_and_side_effects",
        detail: "58 of 58 upstream operations are invoked against both processes with status, normalized body, and side-effect delta captured; 17 operations are exact live differentials — todo 122's five including both SSE streams, plus todo 127's twelve read-only catalogue and filesystem operations — while todos 128 and 129 compare status and normalized body for nineteen more operations; every operation without a local backend remains an explicit 503 gap — the exact set is frozen by FROZEN_API_GAPS and reported by known_gaps(), which derive the count so this prose cannot contradict them — and 8 backed operations carry visible cross-process fixture exemptions; 2 C8 operations are added",
    },
    SurfaceRow {
        id: "config-merge",
        name: "merged configuration across layered trees",
        verdict: Verdict::Compared,
        oracle: OracleKind::LiveBinary,
        evidence: "crates/oc-config/tests/differential.rs::merged_config_matches_real_opencode_across_the_full_matrix",
        detail: "14 trees byte-exact against `opencode debug config` with no normalization; a further 10 layered trees in discovery_differential.rs",
    },
    SurfaceRow {
        id: "config-permission",
        name: "merged permission block, including key order",
        verdict: Verdict::Compared,
        oracle: OracleKind::LiveBinary,
        evidence: "crates/oc-config/tests/differential.rs::permission_env_object_key_order_matches_raw_oracle",
        detail: "OPENCODE_PERMISSION object key order preserved; NOTE the findLast evaluation semantics are unit-tested against the source, not differentially against the binary",
    },
    SurfaceRow {
        id: "agents",
        name: "agent list and resolved agent fields",
        verdict: Verdict::Compared,
        oracle: OracleKind::LiveBinary,
        evidence: "crates/oc-catalog/tests/agent_differential.rs::the_agent_list_matches_real_opencode",
        detail: "live `agent list` against the resolved Rust catalogue",
    },
    SurfaceRow {
        id: "skills",
        name: "skill discovery across every source",
        verdict: Verdict::Compared,
        oracle: OracleKind::LiveBinary,
        evidence: "crates/oc-catalog/tests/skill_differential.rs::debug_skill_matches_the_oracle_across_every_root",
        detail: "live `debug skill` captured to a file because the oracle truncates on a pipe",
    },
    SurfaceRow {
        id: "commands",
        name: "command template expansion",
        verdict: Verdict::Compared,
        oracle: OracleKind::SourceTree,
        evidence: "crates/oc-catalog/tests/command_expansion.rs::command_expansion_matches_the_oracle_on_every_case",
        detail: "the upstream expansion function transcribed to JavaScript and executed by node, so the golden cannot drift from the algorithm it mirrors",
    },
    SurfaceRow {
        id: "tool-registry",
        name: "resolved tool ids per model, flag, and permission",
        verdict: Verdict::Compared,
        oracle: OracleKind::LiveBinary,
        evidence: "crates/oc-tools/tests/registry.rs::registry_resolved_sets_match_five_real_binary_combinations",
        detail: "5 combinations measured from `debug agent <agent> --pure`, compared against captured sets first so the assertion holds without the binary",
    },
    SurfaceRow {
        id: "cli-commands",
        name: "CLI per-command option surface and output",
        verdict: Verdict::Compared,
        oracle: OracleKind::LiveBinary,
        evidence: "crates/oc-cli/tests/differential.rs::every_headless_command_keeps_the_oracle_long_option_surface",
        detail: "29 command/subcommand help surfaces, plus db/models/paths/config/session-listing output comparisons",
    },
    SurfaceRow {
        id: "cli-disposition",
        name: "every upstream command has exactly one disposition",
        verdict: Verdict::Compared,
        oracle: OracleKind::CommittedFixture,
        evidence: "crates/oc-cli/tests/surface.rs::surface_every_upstream_command_has_exactly_one_disposition",
        detail: "23 upstream commands against a committed 1.18.13 registration fixture",
    },
    SurfaceRow {
        id: "paths",
        name: "the nine path keys `debug paths` prints",
        verdict: Verdict::Compared,
        oracle: OracleKind::LiveBinary,
        evidence: "crates/oc-paths/tests/differential.rs::differential_defaults",
        detail: "9 keys byte-exact; the eager-mkdir difference is invisible to this comparison and is declared as no-eager-directory-creation",
    },
    SurfaceRow {
        id: "search",
        name: "ripgrep replacement: file walk and match records",
        verdict: Verdict::Compared,
        oracle: OracleKind::LiveBinary,
        evidence: "crates/oc-tools/tests/search_differential.rs::debug_rg_search_matches_the_embedded_engine_field_for_field",
        detail: "5,007 files in 10 partitions plus 5 search cases; full match records including absolute offsets and submatch spans",
    },
    SurfaceRow {
        id: "lsp-diagnostics",
        name: "LSP diagnostics via a real language server",
        verdict: Verdict::PartiallyCompared,
        oracle: OracleKind::LiveCounterpart,
        evidence: "crates/oc-lsp/tests/live_servers.rs::typescript_diagnostics_match_the_real_opencode_binary",
        detail: "message/severity/code/source/range compared for TypeScript; task 48's evidence records the oracle returning an EMPTY diagnostics array for a Rust fixture, so exact equality on Rust would assert an oracle defect and is deliberately not claimed",
    },
    SurfaceRow {
        id: "session-rows",
        name: "session rows the real binary reads back",
        verdict: Verdict::Compared,
        oracle: OracleKind::LiveBinary,
        evidence: "crates/oc-db/tests/session.rs::the_real_binary_reads_rust_written_sessions_in_the_same_order",
        detail: "ordering, subpath predicate, and post-subtree-delete emptiness, all read by the real binary from a Rust-created database",
    },
    SurfaceRow {
        id: "message-export",
        name: "every message part variant the real binary decodes",
        verdict: Verdict::Compared,
        oracle: OracleKind::LiveBinary,
        evidence: "crates/oc-db/tests/message_export.rs::message_a_rust_written_session_is_readable_by_the_real_binary",
        detail: "`opencode export <id> --pure` strictly decodes a Rust-written session containing every PartKind variant",
    },
    SurfaceRow {
        id: "models-catalog",
        name: "model catalogue listing and provider filtering",
        verdict: Verdict::Compared,
        oracle: OracleKind::LiveBinary,
        evidence: "crates/oc-llm/tests/catalog_differential.rs::parity_across_every_provider_in_the_fixture_at_once",
        detail: "live `models` against a pinned models.dev fixture shared by both sides",
    },
    SurfaceRow {
        id: "v1-compat-surface",
        name: "the measured v1 SDK surface the JavaScript plugin host needs",
        verdict: Verdict::PartiallyCompared,
        oracle: OracleKind::CommittedFixture,
        evidence: "crates/oc-server/tests/compat_v1.rs::compat_v1_every_measured_route_is_reachable_and_never_answers_404",
        detail: "compared against a capture of the oracle's v1 routes and the plugins' measured callsites; the full 67-route v1 surface is deliberately NOT served",
    },
    SurfaceRow {
        id: "execute-parameter-contract",
        name: "the `execute` tool's live parameter schema",
        verdict: Verdict::PartiallyCompared,
        oracle: OracleKind::SourceTree,
        evidence: "crates/oc-testkit/tests/compat_suite.rs::the_execute_tools_live_schema_matches_its_divergence_entry",
        detail: "a declared divergence, VERIFIED: the live schemars-derived schema is compared against what docs/divergences.toml claims, so the contract change cannot drift from its declaration",
    },
    SurfaceRow {
        id: "provider-wire-protocol",
        name: "live provider request/response bytes per wire family",
        verdict: Verdict::NotCompared,
        oracle: OracleKind::None,
        evidence: "(none)",
        detail: "NOT COMPARED against a live provider: the harness has no HTTP client by construction (crates/oc-testkit/Cargo.toml) and plan todo 87 owns cassette-replayed provider parity. Family coverage is a declared divergence, not a measured equality",
    },
    SurfaceRow {
        id: "tui-rendering",
        name: "terminal output byte-for-byte",
        verdict: Verdict::NotCompared,
        oracle: OracleKind::None,
        evidence: "(none)",
        detail: "NOT COMPARED and never will be: the plan's Q1 answer is an equivalent ratatui interface, explicitly not a pixel-identical reproduction of OpenTUI",
    },
    SurfaceRow {
        id: "acp-transport",
        name: "Agent Client Protocol method surface",
        verdict: Verdict::NotCompared,
        oracle: OracleKind::None,
        evidence: "(none)",
        detail: "NOT COMPARED against the real binary: plan todo 78 validates against the real @agentclientprotocol/sdk on disk, which is a live-counterpart check rather than an oracle differential",
    },
];

/// Every volatile value any comparison in this target scrubs, with its reason.
///
/// Deliberately short. A long list is itself a finding: each entry is a licence to
/// differ, and enough of them make any two programs agree. Each one below masks a
/// value whose *identity* carries no meaning — punctuation SQLite is free to
/// re-emit differently, a location the harness itself chose, and a document body
/// this target does not claim to compare.
fn normalizations() -> Vec<Normalization> {
    vec![
        Normalization {
            surface: "db-schema".to_owned(),
            value: "SQL whitespace runs, backtick and double-quote identifier quoting, trailing semicolons, and identifier/type letter case".to_owned(),
            reason: "SQLite re-emits the CREATE statement it was given; quoting and spacing are not part of the schema's meaning, and column/type case is compared case-folded on both sides. Structure — object names, column order, notnull, defaults, foreign-key actions — is compared exactly.".to_owned(),
        },
        Normalization {
            surface: "db-schema, db-migration-journal".to_owned(),
            value: "the temporary directory each side's database is created under".to_owned(),
            reason: "the harness chooses both locations; a path it invented cannot be a compatibility fact. Nothing inside the database records it.".to_owned(),
        },
        Normalization {
            surface: "api-operations".to_owned(),
            value: "generated SSE event id plus session-event timestamp and message id".to_owned(),
            reason: "each process generates those identities independently; the matrix preserves and compares event type, durable aggregate/sequence/version, all stable data, status, and the emitted-event side effect. No response object or semantic field is removed wholesale.".to_owned(),
        },
        Normalization {
            surface: "api-operations: task 128 session-read, request, and PTY attach operations".to_owned(),
            value: "temporary location identity and implementation-specific error envelope".to_owned(),
            reason: "the two isolated servers resolve different temporary roots and encode typed HTTP errors differently; the matrix preserves success data, HTTP status, and whether an error body was returned while dedicated API tests assert exact local payloads, pagination, ticket lifetime, replay rejection, redaction, and terminal I/O.".to_owned(),
        },
    ]
}

// ---------------------------------------------------------------------------
// Oracle plumbing
// ---------------------------------------------------------------------------

/// The pinned oracle, resolved once per test process, or `None` when absent.
///
/// Absence yields `None` so the skip contract still holds. A binary that resolves
/// but reports another release **panics here**, on purpose: continuing would
/// produce a report attributing its measurements to [`PINNED_RELEASE`] while a
/// different build did the work, which is exactly what was rejected. The remedy is
/// in the error.
///
/// Cached because four tests need it and each resolution executes `--version`.
fn resolved_oracle() -> Option<&'static ResolvedOracle> {
    static RESOLVED: OnceLock<Option<ResolvedOracle>> = OnceLock::new();
    RESOLVED
        .get_or_init(|| match oc_testkit::Oracle::discover_pinned() {
            Ok(oracle) => Some(ResolvedOracle {
                program: oracle.program().to_path_buf(),
                version: oracle.reported_version().to_owned(),
            }),
            Err(oc_testkit::TestkitError::BinaryNotFound { .. }) => None,
            Err(mismatch) => panic!("{mismatch}"),
        })
        .as_ref()
}

/// A resolved oracle reduced to the two facts the suite records.
///
/// [`oc_testkit::Oracle`] owns a [`oc_testkit::ScriptedEnv`] whose temporary tree is
/// deleted on drop, so it cannot be cached; the program path and the probed version
/// can be.
struct ResolvedOracle {
    program: PathBuf,
    version: String,
}

fn oracle_binary() -> Option<PathBuf> {
    resolved_oracle().map(|oracle| oracle.program.clone())
}

/// Restated wherever a test skips, so a reader of the output knows what was wanted.
const NO_ORACLE: &str = "no opencode on PATH and no OC_TESTKIT_ORACLE override";

/// Runs `opencode db` under an isolated XDG world, which is what makes it create
/// its database where [`oracle_database`] expects it.
fn run_oracle(binary: &Path, root: &Path, query: &str) -> Output {
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("create isolated oracle home");
    Command::new(binary)
        .args(["db", "--pure", "--format", "json", query])
        .current_dir(root)
        .env("HOME", home)
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env_remove("OPENCODE_DB")
        .output()
        .expect("run the real opencode binary")
}

/// Where the installed 1.18.12 release puts its database under an isolated
/// `XDG_DATA_HOME`. A release channel is not suffixed — see the
/// `opencode-local.db` note in `.omo/notepads/opencode-rust/issues.md`.
fn oracle_database(root: &Path) -> PathBuf {
    root.join("data").join("opencode").join("opencode.db")
}

fn create_rust_database(path: &Path) {
    let mut connection = oc_db::open::open_at(path).expect("open Rust database");
    oc_db::migration::apply(&mut connection).expect("apply Rust schema");
}

// ---------------------------------------------------------------------------
// Schema snapshotting
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
struct Column {
    position: i64,
    name: String,
    declared_type: String,
    not_null: bool,
    default_value: Option<String>,
    primary_key_position: i64,
}

#[derive(Debug, PartialEq, Eq)]
struct ForeignKey {
    id: i64,
    position: i64,
    referenced_table: String,
    from: String,
    to: String,
    on_update: String,
    on_delete: String,
    match_rule: String,
}

/// A schema as three keyed maps, so a difference can be reported by *name*.
///
/// `oc-db/tests/schema.rs` compares flat vectors with `assert_eq!`, which is
/// correct but dumps both schemas on failure. Keying by object name lets this
/// target say "index `session_project_idx` exists only in the Rust database",
/// which is the message the plan's failure scenario asks for.
#[derive(Debug)]
struct SchemaSnapshot {
    objects: BTreeMap<String, String>,
    columns: BTreeMap<String, Vec<Column>>,
    foreign_keys: BTreeMap<String, Vec<ForeignKey>>,
}

fn normalize_sql(sql: &str) -> String {
    sql.replace(['`', '"'], "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(';')
        .to_ascii_lowercase()
}

fn user_tables(connection: &oc_db::Connection) -> Vec<String> {
    let mut statement = connection
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .expect("prepare table inventory");
    statement
        .query_map([], |row| row.get(0))
        .expect("query table inventory")
        .collect::<Result<Vec<_>, _>>()
        .expect("read table inventory")
}

fn schema_snapshot(connection: &oc_db::Connection) -> SchemaSnapshot {
    let mut objects = BTreeMap::new();
    {
        let mut statement = connection
            .prepare(
                "SELECT type, name, sql FROM sqlite_master \
                 WHERE type IN ('table', 'index') \
                   AND name NOT LIKE 'sqlite_%' \
                   AND sql IS NOT NULL",
            )
            .expect("prepare schema object inventory");
        let rows = statement
            .query_map([], |row| {
                Ok((
                    format!("{} {}", row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                    normalize_sql(&row.get::<_, String>(2)?),
                ))
            })
            .expect("query schema objects")
            .collect::<Result<Vec<_>, _>>()
            .expect("read schema objects");
        objects.extend(rows);
    }

    let mut columns = BTreeMap::new();
    let mut foreign_keys = BTreeMap::new();
    for table in user_tables(connection) {
        let mut column_statement = connection
            .prepare(
                "SELECT cid, name, type, \"notnull\", dflt_value, pk \
                 FROM pragma_table_info(?1) ORDER BY cid",
            )
            .expect("prepare column inventory");
        let table_columns = column_statement
            .query_map([&table], |row| {
                Ok(Column {
                    position: row.get(0)?,
                    name: row.get(1)?,
                    declared_type: row.get::<_, String>(2)?.to_ascii_lowercase(),
                    not_null: row.get::<_, i64>(3)? != 0,
                    default_value: row
                        .get::<_, Option<String>>(4)?
                        .map(|value| normalize_sql(&value)),
                    primary_key_position: row.get(5)?,
                })
            })
            .expect("query columns")
            .collect::<Result<Vec<_>, _>>()
            .expect("read columns");
        columns.insert(table.clone(), table_columns);

        let mut foreign_key_statement = connection
            .prepare(
                "SELECT id, seq, \"table\", \"from\", \"to\", on_update, on_delete, \"match\" \
                 FROM pragma_foreign_key_list(?1) ORDER BY id, seq",
            )
            .expect("prepare foreign key inventory");
        let table_keys = foreign_key_statement
            .query_map([&table], |row| {
                Ok(ForeignKey {
                    id: row.get(0)?,
                    position: row.get(1)?,
                    referenced_table: row.get(2)?,
                    from: row.get(3)?,
                    to: row.get(4)?,
                    on_update: row.get::<_, String>(5)?.to_ascii_uppercase(),
                    on_delete: row.get::<_, String>(6)?.to_ascii_uppercase(),
                    match_rule: row.get::<_, String>(7)?.to_ascii_uppercase(),
                })
            })
            .expect("query foreign keys")
            .collect::<Result<Vec<_>, _>>()
            .expect("read foreign keys");
        foreign_keys.insert(table, table_keys);
    }

    SchemaSnapshot {
        objects,
        columns,
        foreign_keys,
    }
}

/// Every way two schemas differ, one line each, named.
fn schema_differences(rust: &SchemaSnapshot, oracle: &SchemaSnapshot) -> Vec<String> {
    let mut out = Vec::new();

    let rust_names: BTreeSet<&String> = rust.objects.keys().collect();
    let oracle_names: BTreeSet<&String> = oracle.objects.keys().collect();
    for missing in oracle_names.difference(&rust_names) {
        out.push(format!(
            "{missing} exists in the database the real binary created but NOT in the Rust database"
        ));
    }
    for extra in rust_names.difference(&oracle_names) {
        out.push(format!(
            "{extra} exists in the Rust database but NOT in the database the real binary created"
        ));
    }
    for name in rust_names.intersection(&oracle_names) {
        let (left, right) = (&rust.objects[*name], &oracle.objects[*name]);
        if left != right {
            out.push(format!(
                "{name} has different SQL\n  rust:   {left}\n  oracle: {right}"
            ));
        }
    }

    for (table, rust_columns) in &rust.columns {
        match oracle.columns.get(table) {
            None => out.push(format!("table {table} is absent from the oracle database")),
            Some(oracle_columns) if oracle_columns != rust_columns => out.push(format!(
                "table {table} has different columns\n  rust:   {rust_columns:?}\n  oracle: {oracle_columns:?}"
            )),
            Some(_) => {}
        }
    }
    for (table, rust_keys) in &rust.foreign_keys {
        if let Some(oracle_keys) = oracle.foreign_keys.get(table)
            && oracle_keys != rust_keys
        {
            out.push(format!(
                "table {table} has different foreign keys\n  rust:   {rust_keys:?}\n  oracle: {oracle_keys:?}"
            ));
        }
    }
    out
}

fn journal_ids(connection: &oc_db::Connection) -> Vec<String> {
    let mut statement = connection
        .prepare("SELECT id FROM migration ORDER BY rowid")
        .expect("prepare migration journal query");
    statement
        .query_map([], |row| row.get(0))
        .expect("query migration journal")
        .collect::<Result<Vec<_>, _>>()
        .expect("read migration journal")
}

// ---------------------------------------------------------------------------
// The headline cases
// ---------------------------------------------------------------------------

#[test]
fn db_schema_matches_a_database_the_real_binary_created() {
    let Some(binary) = oracle_binary() else {
        eprintln!(
            "SKIPPED db_schema_matches_a_database_the_real_binary_created: {NO_ORACLE}; the \
             SQLite schema was NOT compared"
        );
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let rust_path = dir.path().join("rust").join("opencode.db");
    create_rust_database(&rust_path);

    let oracle_root = dir.path().join("oracle");
    let output = run_oracle(&binary, &oracle_root, "SELECT 1 AS opened");
    assert!(
        output.status.success(),
        "the real binary failed to initialise its database: {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let rust = oc_db::Connection::open(&rust_path).expect("open the Rust database");
    let oracle =
        oc_db::Connection::open(oracle_database(&oracle_root)).expect("open the oracle database");
    let rust_snapshot = schema_snapshot(&rust);
    let oracle_snapshot = schema_snapshot(&oracle);

    let differences = schema_differences(&rust_snapshot, &oracle_snapshot);
    assert!(
        differences.is_empty(),
        "the SQLite schema diverges from the database the real binary created:\n  - {}",
        differences.join("\n  - ")
    );
    assert_eq!(
        user_tables(&rust).len(),
        20,
        "19 schema tables plus migration"
    );
    eprintln!(
        "db-schema: no divergence; tables=20 objects={} columns={} foreign_key_tables={}",
        rust_snapshot.objects.len(),
        rust_snapshot.columns.values().map(Vec::len).sum::<usize>(),
        rust_snapshot.foreign_keys.len(),
    );
}

#[test]
fn journal_round_trip_through_the_real_binary_does_not_replay_migrations() {
    let Some(binary) = oracle_binary() else {
        eprintln!(
            "SKIPPED journal_round_trip_through_the_real_binary_does_not_replay_migrations: \
             {NO_ORACLE}; the journal round-trip was NOT run"
        );
        return;
    };
    let root = tempfile::tempdir().expect("tempdir");
    let path = oracle_database(root.path());
    create_rust_database(&path);

    let before = {
        let connection = oc_db::Connection::open(&path).expect("open the Rust-created database");
        journal_ids(&connection)
    };
    assert_eq!(
        before.len(),
        38,
        "the journal must be prefilled before the round-trip"
    );

    let output = run_oracle(
        &binary,
        root.path(),
        "SELECT count(*) AS migration_count FROM migration",
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!(
        "journal round-trip: real opencode exited {} stdout={} stderr={}",
        output.status,
        stdout.trim(),
        stderr.trim()
    );
    assert!(
        output.status.success(),
        "the real binary died opening a Rust-created database — this is the `Effect.orDie` in \
         packages/core/src/database/migration.ts:19-40 replaying migrations onto a current \
         schema:\n{stderr}"
    );
    assert!(
        stdout.contains("38"),
        "the real binary did not report 38 completed migrations: {stdout}"
    );

    let after = {
        let connection = oc_db::Connection::open(&path).expect("reopen after the real binary");
        journal_ids(&connection)
    };
    assert_eq!(
        after, before,
        "the real binary changed the completed migration set"
    );
}

/// Write one session into a database the oracle already created, and return its id.
///
/// The project row is the **oracle's own**, read back rather than invented, because
/// `session list` is scoped to the project it resolves for the directory it runs in:
/// a session under a project id this test made up produces exit 0 and an empty
/// listing, which would silently stop exercising the decoder. Reusing the oracle's
/// project makes the session row the only thing under test.
///
/// `oc_db::session::create` and [`oc_db::session::model_reference`] are the
/// production writers, called directly. Hand-building the `model` JSON here would
/// make this test agree with a fixture rather than with the binary, which is the
/// failure this whole target exists to prevent.
fn write_session_through_this_port(path: &Path) -> String {
    let mut connection = oc_db::open::open_at(path).expect("open the oracle's database");
    let (project_id, worktree): (String, String) = connection
        .query_row(
            "SELECT id, worktree FROM project ORDER BY rowid LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("the oracle resolved a project when it created the database");
    let mut input = oc_db::session::SessionCreate::new(
        "ses_rollbackrollbackrollbackrollb",
        "rollback",
        &project_id,
        &worktree,
        &worktree,
        "Rollback",
        "0.1.0",
    )
    .at(1_780_000_000_000);
    input.agent = Some("build".to_owned());
    input.model = Some(oc_db::session::model_reference("test", "test-model"));
    let transaction = connection.transaction().expect("begin");
    let created = oc_db::session::create(&transaction, &input).expect("write the session");
    transaction.commit().expect("commit");
    created.into_session().id
}

/// Run `session list --format json` on the released binary against `database`.
fn oracle_session_list(binary: &Path, root: &Path, database: &Path) -> Output {
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("create isolated oracle home");
    Command::new(binary)
        .args(["session", "list", "--format", "json"])
        .current_dir(root)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("OPENCODE_DB", database)
        .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
        .output()
        .expect("run the real opencode binary")
}

/// Surviving a Rust database is not the same as reading the rows a Rust turn wrote.
///
/// [`journal_round_trip_through_the_real_binary_does_not_replay_migrations`] opens an
/// **empty** Rust database and checks the `migration` table. That passes whatever
/// this port later writes into `session`, which is precisely how a session row
/// spelled `{"providerID","modelID"}` reached a release: upstream decodes
/// `row.model.id` (`packages/opencode/src/session/session.ts:88-93`), so the missing
/// key fails the whole listing with `Expected string, got undefined` and exit 1.
///
/// This test closes that gap at the suite level, so the suite is the thing that
/// fails. The end-to-end version — a real turn through the production binary, then
/// this same rollback — is `crates/oc-cli/tests/rollback.rs`.
#[test]
fn a_session_written_by_this_port_is_decodable_by_the_real_binary() {
    let Some(binary) = oracle_binary() else {
        eprintln!(
            "SKIPPED a_session_written_by_this_port_is_decodable_by_the_real_binary: \
             {NO_ORACLE}; the session-decode seam was NOT compared"
        );
        return;
    };
    let root = tempfile::tempdir().expect("tempdir");
    let path = root.path().join("rollback.db");

    let initial = oracle_session_list(&binary, root.path(), &path);
    assert!(
        initial.status.success(),
        "the real binary could not create its own database, so this test never \
         reached the seam it exists for:\n{}",
        String::from_utf8_lossy(&initial.stderr)
    );
    let session_id = write_session_through_this_port(&path);

    let stored: String = {
        let connection = oc_db::Connection::open(&path).expect("reopen the written database");
        connection
            .query_row(
                "SELECT model FROM session WHERE id = ?1",
                [&session_id],
                |row| row.get(0),
            )
            .expect("read the persisted model column")
    };

    let output = oracle_session_list(&binary, root.path(), &path);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!(
        "db-session-decode: real opencode exited {} session.model={stored} stdout={} stderr={}",
        output.status,
        stdout.trim(),
        stderr.trim()
    );
    assert!(
        output.status.success(),
        "the real binary could not read a session this port wrote. `session.model` \
         was {stored}; upstream decodes it as `row.model.id` (session.ts:88-93), so a \
         row spelled `modelID` takes the whole listing down.\nexit: {}\nstdout:\n\
         {stdout}\nstderr:\n{stderr}",
        output.status
    );
    assert!(
        stdout.contains(session_id.as_str()),
        "the real binary exited 0 but did not list the session this port wrote:\n{stdout}"
    );
}

/// The `(path, method)` pairs an OpenAPI document declares under `/api/`.
fn api_operations(document: &serde_json::Value) -> BTreeSet<(String, String)> {
    let mut operations = BTreeSet::new();
    for (path, item) in document["paths"]
        .as_object()
        .expect("an OpenAPI document must have a paths object")
    {
        if !path.starts_with("/api/") {
            continue;
        }
        for method in item.as_object().into_iter().flatten().map(|(name, _)| name) {
            if matches!(method.as_str(), "get" | "post" | "put" | "delete" | "patch") {
                operations.insert((path.clone(), method.clone()));
            }
        }
    }
    operations
}

#[derive(Debug, PartialEq)]
struct ApiObservation {
    status: u16,
    normalized_body: serde_json::Value,
    side_effect: serde_json::Value,
}

fn compare_api_observation(
    operation: &str,
    oracle: &ApiObservation,
    subject: &ApiObservation,
) -> Result<(), String> {
    if subject.status == 501 {
        return Err(format!(
            "{operation} is registered but returned 501; reachability is not behavioural parity"
        ));
    }
    (oracle == subject)
        .then_some(())
        .ok_or_else(|| format!("{operation} differs: oracle={oracle:?} subject={subject:?}"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApiDimension {
    Compared(&'static str),
    Exempt(&'static str),
}

#[derive(Debug)]
struct ApiBehaviourRow {
    path: String,
    method: String,
    group: String,
    status: ApiDimension,
    body: ApiDimension,
    side_effect: ApiDimension,
}

fn api_behaviour_matrix(document: &serde_json::Value) -> Vec<ApiBehaviourRow> {
    let compared = BTreeSet::from([
        ("/api/event", "get"),
        ("/api/health", "get"),
        ("/api/session", "get"),
        ("/api/session/active", "get"),
        ("/api/session/{sessionID}/event", "get"),
        // Todo 127's twelve. Each is a real backend answering a real body, and each
        // is compared against the released binary for status *and* normalized body
        // rather than exempted, which is the whole point of implementing them.
        ("/api/agent", "get"),
        ("/api/model", "get"),
        ("/api/command", "get"),
        ("/api/skill", "get"),
        ("/api/reference", "get"),
        ("/api/provider", "get"),
        ("/api/provider/{providerID}", "get"),
        ("/api/integration", "get"),
        ("/api/integration/{integrationID}", "get"),
        ("/api/fs/read/*", "get"),
        ("/api/fs/list", "get"),
        ("/api/fs/find", "get"),
    ]);
    let task_128_compared = BTreeSet::from([
        ("/api/session/{sessionID}/context", "get"),
        ("/api/session/{sessionID}/history", "get"),
        ("/api/session/{sessionID}/message", "get"),
        ("/api/session/{sessionID}/question", "get"),
        ("/api/permission/request", "get"),
        ("/api/permission/saved", "get"),
        ("/api/permission/saved/{id}", "delete"),
        ("/api/question/request", "get"),
        ("/api/pty/{ptyID}/connect-token", "post"),
        ("/api/pty/{ptyID}/connect", "get"),
    ]);
    // `/compact` and `/wait` are deliberately absent. The isolated oracle answers 503
    // for both in this fixture -- each needs a provider-backed run the harness does not
    // give it -- so a Compared status dimension would assert against the oracle's own
    // gap rather than against upstream behaviour. Our 204s are covered by the dedicated
    // server and CLI tests instead. Claiming a comparison the fixture cannot support is
    // exactly the "honest gap reported as parity" defect the Final Wave rejected.
    let task_129_compared = BTreeSet::from([
        ("/api/session/{sessionID}/prompt", "post"),
        ("/api/session/{sessionID}/interrupt", "post"),
        ("/api/session/{sessionID}/agent", "post"),
        ("/api/session/{sessionID}/model", "post"),
        ("/api/session/{sessionID}/revert/stage", "post"),
        ("/api/session/{sessionID}/revert/clear", "post"),
        ("/api/session/{sessionID}/revert/commit", "post"),
    ]);
    let task_132_compared = BTreeSet::from([
        ("/api/session/{sessionID}/permission", "get"),
        (
            "/api/session/{sessionID}/permission/{requestID}/reply",
            "post",
        ),
        (
            "/api/session/{sessionID}/question/{requestID}/reply",
            "post",
        ),
        (
            "/api/session/{sessionID}/question/{requestID}/reject",
            "post",
        ),
    ]);
    let mut rows = Vec::new();
    for (path, item) in document["paths"]
        .as_object()
        .expect("the oracle OpenAPI has paths")
    {
        if !path.starts_with("/api/") {
            continue;
        }
        for (method, operation) in item.as_object().expect("a path item is an object") {
            if !matches!(method.as_str(), "get" | "post" | "put" | "delete" | "patch") {
                continue;
            }
            let group = operation["tags"]
                .as_array()
                .and_then(|tags| tags.first())
                .and_then(serde_json::Value::as_str)
                .unwrap_or("untagged")
                .to_owned();
            let key = (path.as_str(), method.as_str());
            let (status, body, side_effect) = if compared.contains(&key) {
                let evidence = match key {
                    ("/api/event", "get") => "live first SSE frame plus local publish delivery",
                    ("/api/session/{sessionID}/event", "get") => {
                        "live durable SSE frame after sequence zero"
                    }
                    ("/api/agent", "get")
                    | ("/api/command", "get")
                    | ("/api/skill", "get")
                    | ("/api/reference", "get") => {
                        "live catalogue request against a shared isolated config world"
                    }
                    ("/api/model", "get")
                    | ("/api/provider", "get")
                    | ("/api/provider/{providerID}", "get")
                    | ("/api/integration", "get")
                    | ("/api/integration/{integrationID}", "get") => {
                        "live catalogue request against a pinned models.dev document and one declared credential"
                    }
                    ("/api/fs/read/*", "get")
                    | ("/api/fs/list", "get")
                    | ("/api/fs/find", "get") => {
                        "live filesystem request against an identically seeded worktree"
                    }
                    _ => "live isolated empty-state request",
                };
                (
                    ApiDimension::Compared(evidence),
                    ApiDimension::Compared(evidence),
                    ApiDimension::Compared(evidence),
                )
            } else if task_128_compared.contains(&key) {
                (
                    ApiDimension::Compared("live status against the isolated upstream process"),
                    ApiDimension::Compared(
                        "live operation-scoped normalized body against the isolated upstream process",
                    ),
                    ApiDimension::Exempt(
                        "process-local request and PTY state is verified by dedicated oc-server API tests",
                    ),
                )
            } else if task_129_compared.contains(&key) {
                (
                    ApiDimension::Compared("live status against the isolated upstream process"),
                    ApiDimension::Compared(
                        "live operation-scoped normalized body against the isolated upstream process",
                    ),
                    ApiDimension::Exempt(
                        "turn execution, cancellation, waiting, and durable mutation are verified by dedicated server and CLI tests",
                    ),
                )
            } else if task_132_compared.contains(&key) {
                (
                    ApiDimension::Compared("live status against the isolated upstream process"),
                    ApiDimension::Compared(
                        "live operation-scoped normalized body against the isolated upstream process",
                    ),
                    ApiDimension::Exempt(
                        "pending request resolution is verified by dedicated server and production HTTP-turn tests",
                    ),
                )
            } else {
                let reason = api_exemption_reason(path, method, &group);
                (
                    ApiDimension::Exempt(reason),
                    ApiDimension::Exempt(reason),
                    ApiDimension::Exempt(reason),
                )
            };
            rows.push(ApiBehaviourRow {
                path: path.clone(),
                method: method.clone(),
                group,
                status,
                body,
                side_effect,
            });
        }
    }
    rows.sort_by(|left, right| (&left.path, &left.method).cmp(&(&right.path, &right.method)));
    rows
}

fn api_exemption_reason(path: &str, method: &str, group: &str) -> &'static str {
    if (path, method) == ("/api/session/{sessionID}/compact", "post") {
        return "the isolated upstream oracle returns 503 without a provider, so its unavailable path cannot prove compaction status or side-effect parity";
    }
    match group {
        "integrations" => {
            "requires provider credentials, OAuth callbacks, or an attempt identity that cannot be shared between isolated processes"
        }
        "permissions" | "session questions" => {
            "requires a pending process-local request whose identity cannot be seeded through the public HTTP surface"
        }
        "providers" | "models" | "commands" | "skills" | "reference" => {
            "depends on host configuration or a live catalogue; the two bound servers have no shared catalogue-injection seam"
        }
        "filesystem" => {
            "requires a shared scripted worktree and path normalization; no released-binary filesystem fixture is committed"
        }
        "pty" if path.ends_with("/connect") => {
            "requires a WebSocket frame runner; the harness intentionally has only raw HTTP and bounded SSE capture"
        }
        "pty" => {
            "mutates or addresses an OS PTY with process-specific IDs; no cross-process deterministic PTY fixture exists"
        }
        "sessions" if method != "get" => {
            "mutates provider, revert, or run state whose generated IDs and filesystem snapshot cannot be seeded identically"
        }
        "sessions" => {
            "requires matching session/message/history rows in both isolated databases; this operation has no shared seed fixture"
        }
        "opencode HttpApi" if path.starts_with("/api/credential/") => {
            "mutates isolated credential storage and requires a stable credential identity without reading the user's auth file"
        }
        "opencode HttpApi" => {
            "response depends on the independently resolved project/location or catalogue and has no shared injection seam"
        }
        _ => "no deterministic cross-process fixture exists for this operation",
    }
}

struct OracleServer {
    child: Child,
    addr: SocketAddr,
}

struct SubjectServer {
    addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Debug)]
struct MatrixMutationExecutor;

impl oc_server::SessionMutationExecutor for MatrixMutationExecutor {
    fn prompt(
        &self,
        _request: oc_server::SessionPromptExecution,
        _interrupt: oc_engine::interrupt::InterruptSignal,
        _events: oc_engine::r#loop::TurnEventSender,
    ) -> oc_server::SessionMutationFuture {
        Box::pin(async { Ok(()) })
    }

    fn compact(
        &self,
        _request: oc_server::SessionCompactExecution,
        _interrupt: oc_engine::interrupt::InterruptSignal,
    ) -> oc_server::SessionMutationFuture {
        Box::pin(async { Ok(()) })
    }
}

impl Drop for SubjectServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start_subject_server(root: &Path) -> SubjectServer {
    let event_pool =
        Arc::new(oc_db::Pool::open(&oc_paths::DbLocation::Memory).expect("subject event database"));
    let events = oc_server::EventService::new(Arc::clone(&event_pool), 8);
    let work = matrix_worktree(root);
    let state = oc_server::api::ApiState::memory(work.to_string_lossy())
        .expect("subject API state")
        .with_env(oc_paths::Env::from_pairs(matrix_env(root)))
        .with_events(events.clone());
    let subject = oc_server::ServerBuilder::new(
        oc_server::ServerConfig::default()
            .with_port(0)
            .with_default_directory(work.to_string_lossy()),
    )
    .with_services(
        oc_server::ServerServices::new(8).with_mutations(Arc::new(MatrixMutationExecutor)),
    )
    .with_routes(oc_server::api::router(state).merge(oc_server::events_router(events)))
    .bind()
    .await
    .expect("bind subject matrix server");
    let addr = subject.local_addr();
    SubjectServer {
        addr,
        task: tokio::spawn(async move {
            let _ = subject.serve().await;
        }),
    }
}

impl Drop for OracleServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The pinned models.dev document both matrix servers resolve their catalogue from.
///
/// Without it the two processes read different caches — the oracle would fetch from
/// models.dev and the subject would find an empty cache — and `/api/provider`,
/// `/api/model` and `/api/integration` would differ for reasons that have nothing
/// to do with this port.
const MATRIX_CATALOGUE_FIXTURE: &str = "crates/oc-llm/tests/fixtures/models-dev-pinned.json";

/// The one credential the matrix world declares, so exactly one provider resolves
/// as available on both sides.
const MATRIX_CREDENTIAL_ENV: (&str, &str) = ("DEEPSEEK_API_KEY", "matrix-fixture-key");

/// Seeds the directory both matrix servers serve and returns it.
///
/// The served worktree is a `work/` **subdirectory** rather than the state root:
/// the oracle creates `home/`, `data/` and `cache/` inside its root, the subject
/// does not, and `/api/fs/list` would then disagree about the contents of a
/// directory whose difference is the harness's own doing.
fn matrix_worktree(root: &Path) -> PathBuf {
    let work = root.join("work");
    std::fs::create_dir_all(work.join("nested")).expect("create the matrix worktree");
    std::fs::write(work.join("Cargo.toml"), b"[package]\nname = \"matrix\"\n")
        .expect("seed the matrix file the fs/read path names");
    std::fs::write(work.join("alpha.txt"), b"alpha\n").expect("seed a matrix file");
    std::fs::write(work.join("nested").join("deep.txt"), b"deep\n")
        .expect("seed a nested matrix file");
    work
}

/// The `(key, value)` environment both matrix servers run under.
fn matrix_env(root: &Path) -> Vec<(String, String)> {
    let catalogue = oc_testkit::subject::workspace_root()
        .expect("workspace root")
        .join(MATRIX_CATALOGUE_FIXTURE);
    vec![
        ("HOME".to_owned(), path_string(&root.join("home"))),
        ("XDG_DATA_HOME".to_owned(), path_string(&root.join("data"))),
        (
            "XDG_CONFIG_HOME".to_owned(),
            path_string(&root.join("config")),
        ),
        (
            "XDG_CACHE_HOME".to_owned(),
            path_string(&root.join("cache")),
        ),
        (
            "XDG_STATE_HOME".to_owned(),
            path_string(&root.join("state")),
        ),
        ("OPENCODE_MODELS_PATH".to_owned(), path_string(&catalogue)),
        ("OPENCODE_DISABLE_MODELS_FETCH".to_owned(), "1".to_owned()),
        (
            MATRIX_CREDENTIAL_ENV.0.to_owned(),
            MATRIX_CREDENTIAL_ENV.1.to_owned(),
        ),
    ]
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn unused_loopback_port() -> u16 {
    StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .expect("reserve an oracle port")
        .local_addr()
        .expect("read reserved port")
        .port()
}

fn start_oracle_server(binary: &Path, root: &Path) -> OracleServer {
    let port = unused_loopback_port();
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("create isolated oracle home");
    let work = matrix_worktree(root);
    let mut child = Command::new(binary)
        .args([
            "serve",
            "--hostname",
            "127.0.0.1",
            "--port",
            &port.to_string(),
        ])
        .current_dir(&work)
        .envs(matrix_env(root))
        .env_remove("OPENCODE_DB")
        .env_remove("OPENCODE_SERVER_PASSWORD")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start released opencode server");
    let stdout = child.stdout.take().expect("oracle stdout is piped");
    let expected = format!("http://127.0.0.1:{port}");
    let mut ready = false;
    for line in BufReader::new(stdout).lines().take(8) {
        if line.expect("read oracle startup line").contains(&expected) {
            ready = true;
            break;
        }
    }
    assert!(ready, "released server did not report {expected}");
    OracleServer {
        child,
        addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
    }
}

async fn raw_http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&str>,
    first_sse_frame: bool,
) -> (u16, String) {
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect to matrix server");
    let body = body.unwrap_or("");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n{body}",
        body.len(),
        if first_sse_frame {
            "keep-alive"
        } else {
            "close"
        }
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write matrix request");
    let mut raw = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut chunk))
            .await
            .unwrap_or_else(|_| panic!("{method} {path} response timeout"))
            .expect("read matrix response");
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);
        if http_response_complete(&raw, first_sse_frame) {
            break;
        }
    }
    let text = String::from_utf8_lossy(&raw);
    let (head, raw_body) = text.split_once("\r\n\r\n").unwrap_or_else(|| {
        panic!("{method} {path} response has no HTTP header terminator: {text:?}")
    });
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse().ok())
        .expect("HTTP status code");
    let chunked = head
        .lines()
        .any(|line| line.eq_ignore_ascii_case("transfer-encoding: chunked"));
    let body = if chunked {
        decode_chunked(raw_body, first_sse_frame)
    } else {
        raw_body.to_owned()
    };
    (status, body)
}

fn http_response_complete(raw: &[u8], first_sse_frame: bool) -> bool {
    let text = String::from_utf8_lossy(raw);
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        return false;
    };
    if first_sse_frame {
        return body.contains("\n\n");
    }
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1));
    if matches!(status, Some("204" | "304")) {
        return true;
    }
    if let Some(length) = head.lines().find_map(|line| {
        line.to_ascii_lowercase()
            .strip_prefix("content-length: ")
            .and_then(|value| value.parse::<usize>().ok())
    }) {
        return body.len() >= length;
    }
    head.lines()
        .any(|line| line.eq_ignore_ascii_case("transfer-encoding: chunked"))
        && body.ends_with("0\r\n\r\n")
}

fn decode_chunked(mut raw: &str, first_only: bool) -> String {
    let mut decoded = String::new();
    while let Some((length, rest)) = raw.split_once("\r\n") {
        let Ok(length) = usize::from_str_radix(length.trim(), 16) else {
            break;
        };
        if length == 0 || rest.len() < length {
            break;
        }
        decoded.push_str(&rest[..length]);
        if first_only {
            break;
        }
        raw = rest.get(length + 2..).unwrap_or_default();
    }
    decoded
}

fn normalize_sse(frame: &str) -> serde_json::Value {
    let data = frame
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .expect("SSE frame has data");
    let mut value: serde_json::Value = serde_json::from_str(data).expect("SSE data is JSON");
    if let Some(object) = value.as_object_mut() {
        object.remove("id");
        if let Some(data) = object
            .get_mut("data")
            .and_then(serde_json::Value::as_object_mut)
        {
            data.remove("timestamp");
            data.remove("messageID");
        }
    }
    value
}

fn concrete_api_path(path: &str) -> String {
    path.replace("{sessionID}", "ses_matrix")
        .replace("{ptyID}", "pty_matrix")
        .replace("{providerID}", "provider_matrix")
        .replace("{integrationID}", "integration_matrix")
        .replace("{attemptID}", "attempt_matrix")
        .replace("{credentialID}", "credential_matrix")
        .replace("{requestID}", "request_matrix")
        .replace("{messageID}", "msg_matrix")
        .replace("{id}", "saved_matrix")
        .replace("/*", "/Cargo.toml")
}

fn api_request_body(row: &ApiBehaviourRow) -> Option<&'static str> {
    match (row.method.as_str(), row.path.as_str()) {
        ("post", "/api/session") => Some(r#"{"id":"ses_matrix"}"#),
        ("post", "/api/session/{sessionID}/agent") => Some(r#"{"agent":"plan"}"#),
        ("post", "/api/session/{sessionID}/model") => {
            Some(r#"{"model":{"providerID":"deepseek","id":"deepseek-chat"}}"#)
        }
        ("post", "/api/session/{sessionID}/prompt") => Some(
            r#"{"id":"msg_matrix_prompt","prompt":{"text":"matrix","files":[],"agents":[]},"delivery":"steer","resume":false}"#,
        ),
        ("post", "/api/session/{sessionID}/revert/stage") => {
            Some(r#"{"messageID":"msg_matrix","files":false}"#)
        }
        ("post", "/api/pty") => Some(r#"{"command":"/definitely/not-an-executable"}"#),
        ("post" | "put" | "patch", _) => Some("{}"),
        _ => None,
    }
}

fn normalize_http_body(body: &str, sse: bool) -> serde_json::Value {
    if sse {
        return normalize_sse(body);
    }
    let body = body.trim();
    if body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(body).unwrap_or_else(|_| serde_json::Value::String(body.to_owned()))
    }
}

fn normalize_scoped_api_body(
    row: &ApiBehaviourRow,
    status: u16,
    body: &str,
    sse: bool,
) -> serde_json::Value {
    let mut value = normalize_http_body(body, sse);
    let task_128 = matches!(
        (row.method.as_str(), row.path.as_str()),
        ("get", "/api/session/{sessionID}/context")
            | ("get", "/api/session/{sessionID}/history")
            | ("get", "/api/session/{sessionID}/message")
            | ("get", "/api/session/{sessionID}/question")
            | ("get", "/api/permission/request")
            | ("get", "/api/permission/saved")
            | ("delete", "/api/permission/saved/{id}")
            | ("get", "/api/question/request")
            | ("post", "/api/pty/{ptyID}/connect-token")
            | ("get", "/api/pty/{ptyID}/connect")
    );
    let task_129 = matches!(
        (row.method.as_str(), row.path.as_str()),
        ("post", "/api/session/{sessionID}/prompt")
            | ("post", "/api/session/{sessionID}/compact")
            | ("post", "/api/session/{sessionID}/wait")
            | ("post", "/api/session/{sessionID}/interrupt")
            | ("post", "/api/session/{sessionID}/agent")
            | ("post", "/api/session/{sessionID}/model")
            | ("post", "/api/session/{sessionID}/revert/stage")
            | ("post", "/api/session/{sessionID}/revert/clear")
            | ("post", "/api/session/{sessionID}/revert/commit")
    );
    let task_132 = matches!(
        (row.method.as_str(), row.path.as_str()),
        ("get", "/api/session/{sessionID}/permission")
            | (
                "post",
                "/api/session/{sessionID}/permission/{requestID}/reply"
            )
            | (
                "post",
                "/api/session/{sessionID}/question/{requestID}/reply"
            )
            | (
                "post",
                "/api/session/{sessionID}/question/{requestID}/reject"
            )
    );
    if !task_128 && !task_129 && !task_132 {
        return value;
    }
    if status >= 400 {
        return serde_json::json!({
            "error": {
                "status": status
            }
        });
    }
    if let Some(object) = value.as_object_mut() {
        if task_128 {
            object.remove("location");
        }
        if task_128
            && row.path == "/api/pty/{ptyID}/connect-token"
            && let Some(data) = object
                .get_mut("data")
                .and_then(serde_json::Value::as_object_mut)
        {
            data.remove("ticket");
        }
        if task_128
            && matches!(
                row.path.as_str(),
                "/api/session/{sessionID}/context" | "/api/session/{sessionID}/message"
            )
        {
            let has_switch_message = object
                .get_mut("data")
                .and_then(serde_json::Value::as_array_mut)
                .is_some_and(|messages| normalize_switch_messages(messages));
            if has_switch_message && row.path == "/api/session/{sessionID}/message" {
                object.remove("cursor");
            }
        }
        if task_128
            && row.path == "/api/session/{sessionID}/history"
            && let Some(events) = object
                .get_mut("data")
                .and_then(serde_json::Value::as_array_mut)
        {
            for event in events {
                if matches!(
                    event.get("type").and_then(serde_json::Value::as_str),
                    Some("session.next.agent.switched" | "session.next.model.switched")
                ) && let Some(event) = event.as_object_mut()
                {
                    event.remove("id");
                    if let Some(data) = event
                        .get_mut("data")
                        .and_then(serde_json::Value::as_object_mut)
                    {
                        data.remove("timestamp");
                        data.remove("messageID");
                    }
                }
            }
        }
        if task_129
            && row.path == "/api/session/{sessionID}/prompt"
            && let Some(data) = object
                .get_mut("data")
                .and_then(serde_json::Value::as_object_mut)
        {
            data.remove("admittedSeq");
            data.remove("timeCreated");
        }
    }
    value
}

fn normalize_switch_messages(messages: &mut [serde_json::Value]) -> bool {
    let mut found = false;
    for message in messages {
        if matches!(
            message.get("type").and_then(serde_json::Value::as_str),
            Some("agent-switched" | "model-switched")
        ) && let Some(message) = message.as_object_mut()
        {
            found = true;
            message.remove("id");
            if let Some(time) = message
                .get_mut("time")
                .and_then(serde_json::Value::as_object_mut)
            {
                time.remove("created");
            }
        }
    }
    found
}

async fn observable_api_state(addr: SocketAddr) -> serde_json::Value {
    let sessions = raw_http(addr, "GET", "/api/session", None, false).await;
    let ptys = raw_http(addr, "GET", "/api/pty", None, false).await;
    serde_json::json!({
        "sessions": normalize_http_body(&sessions.1, false),
        "ptys": normalize_http_body(&ptys.1, false),
    })
}

/// Replaces one process's own state root with a token so the two can be compared.
///
/// Both servers report absolute paths that name their own tempdir — the agent
/// ruleset names the tool-output and plans directories, and `location.directory`
/// names the served worktree. Those differ *by construction*, so the root and its
/// final component are tokenized. Nothing else is normalized: this is a substitution
/// of the harness's own two variables, not a smoother that would make unequal
/// bodies compare equal.
fn normalize_state_paths(value: serde_json::Value, root: &Path) -> serde_json::Value {
    let root = root.to_string_lossy().into_owned();
    let leaf = Path::new(&root)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    fn walk(value: serde_json::Value, root: &str, leaf: &str) -> serde_json::Value {
        match value {
            serde_json::Value::String(text) => {
                let text = text.replace(root, "<MATRIX_ROOT>");
                // The plan-edit rule is `path.relative(worktree, …)`, which drops the
                // absolute prefix and leaves only `../<leaf>/…`.
                let text = if leaf.is_empty() {
                    text
                } else {
                    text.replace(&format!("../{leaf}/"), "../<MATRIX_ROOT>/")
                };
                serde_json::Value::String(text)
            }
            serde_json::Value::Array(items) => serde_json::Value::Array(
                items
                    .into_iter()
                    .map(|item| walk(item, root, leaf))
                    .collect(),
            ),
            serde_json::Value::Object(fields) => serde_json::Value::Object(
                fields
                    .into_iter()
                    .map(|(key, item)| (key, walk(item, root, leaf)))
                    .collect(),
            ),
            other => other,
        }
    }
    walk(value, &root, &leaf)
}

/// Waits until the oracle's catalogue services have finished initializing.
///
/// **The released binary answers `[]` for `/api/agent`, `/api/command` and
/// `/api/provider` for the first moments after it starts listening**, then converges
/// on the real roster. Measured on 1.18.12: the first request after startup returned
/// zero agents and zero commands, and a later one returned seven and two. A
/// differential that queries it cold therefore reports differences that are the
/// oracle disagreeing with itself, so it is warmed before anything is compared.
async fn warm_oracle(addr: SocketAddr) {
    for _ in 0..40 {
        let agents = raw_http(addr, "GET", "/api/agent", None, false).await;
        let commands = raw_http(addr, "GET", "/api/command", None, false).await;
        let seeded = |body: &str| {
            serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .and_then(|value| value["data"].as_array().map(|items| !items.is_empty()))
                .unwrap_or(false)
        };
        if seeded(&agents.1) && seeded(&commands.1) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!("the released server never populated its agent and command catalogues");
}

async fn observe_api_operation(
    addr: SocketAddr,
    row: &ApiBehaviourRow,
    root: &Path,
) -> ApiObservation {
    let path = concrete_api_path(&row.path);
    let sse = matches!(
        row.path.as_str(),
        "/api/event" | "/api/session/{sessionID}/event"
    );
    let path = if row.path == "/api/session/{sessionID}/event" {
        format!("{path}?after=0")
    } else {
        path
    };
    let before = observable_api_state(addr).await;
    let response = raw_http(
        addr,
        &row.method.to_ascii_uppercase(),
        &path,
        api_request_body(row),
        sse,
    )
    .await;
    let after = observable_api_state(addr).await;
    ApiObservation {
        status: response.0,
        normalized_body: normalize_state_paths(
            normalize_scoped_api_body(row, response.0, &response.1, sse),
            root,
        ),
        side_effect: serde_json::json!({"changed": before != after}),
    }
}

fn compare_selected_api_dimensions(
    row: &ApiBehaviourRow,
    oracle: &ApiObservation,
    subject: &ApiObservation,
) -> Result<(), String> {
    let operation = format!("{} {}", row.method.to_ascii_uppercase(), row.path);
    if matches!(row.status, ApiDimension::Compared(_)) && oracle.status != subject.status {
        return Err(format!(
            "{operation} status differs: oracle={} subject={}",
            oracle.status, subject.status
        ));
    }
    if matches!(row.body, ApiDimension::Compared(_))
        && oracle.normalized_body != subject.normalized_body
    {
        return Err(format!(
            "{operation} body differs: oracle={} subject={}",
            oracle.normalized_body, subject.normalized_body
        ));
    }
    if matches!(row.side_effect, ApiDimension::Compared(_))
        && oracle.side_effect != subject.side_effect
    {
        return Err(format!(
            "{operation} side effect differs: oracle={} subject={}",
            oracle.side_effect, subject.side_effect
        ));
    }
    Ok(())
}

fn assert_subject_operation_is_accounted(row: &ApiBehaviourRow, subject: &ApiObservation) {
    assert_ne!(
        subject.status,
        501,
        "{} {} is registered but still only a 501 stub",
        row.method.to_ascii_uppercase(),
        row.path
    );
    // `fs/read` answers raw file bytes by design (`protocol/src/groups/fs.ts:22-24`
    // declares its success as `Uint8Array`), so it is the one operation whose body is
    // legitimately not JSON. Every other route must still answer JSON or nothing.
    if row.path != "/api/fs/read/*" {
        assert!(
            !subject.normalized_body.is_string(),
            "{} {} returned a non-JSON, non-empty body: {}",
            row.method.to_ascii_uppercase(),
            row.path,
            subject.normalized_body
        );
    }
    if subject.status == 503 {
        assert_eq!(
            subject.normalized_body["error"]["code"],
            "backend_unavailable",
            "{} {} must name its explicit gap",
            row.method.to_ascii_uppercase(),
            row.path
        );
        assert_eq!(subject.side_effect["changed"], false);
        assert!(
            matches!(row.status, ApiDimension::Exempt(_))
                && matches!(row.body, ApiDimension::Exempt(_))
                && matches!(row.side_effect, ApiDimension::Exempt(_)),
            "an unavailable backend may not be reported as compared"
        );
    }
}

#[test]
fn api_behaviour_matrix_rejects_a_501_only_operation() {
    let stub = ApiObservation {
        status: 501,
        normalized_body: serde_json::json!({"error":{"code":"not_implemented"}}),
        side_effect: serde_json::json!({"changed": false}),
    };
    let error = compare_api_observation("GET /api/integration", &stub, &stub)
        .expect_err("a registered 501 stub must never satisfy behavioural parity");
    assert!(
        error.contains("501"),
        "the rejection must name the stub status"
    );
}

#[test]
fn api_behaviour_matrix_accounts_for_status_body_and_side_effect_per_operation() {
    let root = oc_testkit::subject::workspace_root().expect("workspace root");
    let document: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.join(ORACLE_OPENAPI_FIXTURE)).expect("read oracle OpenAPI"),
    )
    .expect("parse oracle OpenAPI");
    let operations = api_operations(&document);
    let matrix = api_behaviour_matrix(&document);
    let matrix_operations = matrix
        .iter()
        .map(|row| (row.path.clone(), row.method.clone()))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        matrix.len(),
        UPSTREAM_API_OPERATIONS,
        "the behaviour matrix must have one row per operation"
    );
    assert_eq!(
        matrix_operations, operations,
        "the matrix may neither omit nor invent operations"
    );

    let mut compared = 0;
    let mut exempted = 0;
    for row in &matrix {
        let dimensions = [row.status, row.body, row.side_effect];
        for dimension in dimensions {
            match dimension {
                ApiDimension::Compared(evidence) => {
                    assert!(!evidence.trim().is_empty());
                    compared += 1;
                }
                ApiDimension::Exempt(reason) => {
                    assert!(!reason.trim().is_empty());
                    exempted += 1;
                }
            }
        }
        if matches!(row.status, ApiDimension::Exempt(_)) {
            eprintln!(
                "api-matrix exemption: {} {} [{}] — {}",
                row.method.to_ascii_uppercase(),
                row.path,
                row.group,
                match row.status {
                    ApiDimension::Exempt(reason) => reason,
                    ApiDimension::Compared(_) => unreachable!(),
                }
            );
        }
    }
    assert_eq!(
        compared, 93,
        "thirty-eight live operations are compared: todo 122's five and todo 127's twelve compare all three dimensions, while todo 128's ten, todo 129's seven, and todo 132's four compare status and normalized body -- /compact and /wait are exempt because the isolated oracle answers 503 for both"
    );
    assert_eq!(
        exempted, 81,
        "every other dimension must carry a visible reason, including all three compact dimensions"
    );
}

#[tokio::test]
async fn api_behaviour_matrix_invokes_every_subject_operation_and_rejects_501() {
    let root = oc_testkit::subject::workspace_root().expect("workspace root");
    let document: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.join(ORACLE_OPENAPI_FIXTURE)).expect("read oracle OpenAPI"),
    )
    .expect("parse oracle OpenAPI");
    let matrix = api_behaviour_matrix(&document);
    let subject_root = tempfile::tempdir().expect("subject matrix root");
    let subject = start_subject_server(subject_root.path()).await;
    let mut invoked = BTreeSet::new();
    let mut unavailable = 0;
    let mut task_129_unavailable = 0;
    for row in &matrix {
        let observation = observe_api_operation(subject.addr, row, subject_root.path()).await;
        assert_subject_operation_is_accounted(row, &observation);
        unavailable += usize::from(observation.status == 503);
        if matches!(
            row.path.as_str(),
            "/api/session/{sessionID}/prompt"
                | "/api/session/{sessionID}/compact"
                | "/api/session/{sessionID}/wait"
                | "/api/session/{sessionID}/interrupt"
                | "/api/session/{sessionID}/agent"
                | "/api/session/{sessionID}/model"
                | "/api/session/{sessionID}/revert/stage"
                | "/api/session/{sessionID}/revert/clear"
                | "/api/session/{sessionID}/revert/commit"
        ) {
            task_129_unavailable += usize::from(observation.status == 503);
        }
        assert!(
            invoked.insert((row.path.clone(), row.method.clone())),
            "matrix invoked an operation twice"
        );
    }
    assert_eq!(
        invoked.len(),
        UPSTREAM_API_OPERATIONS,
        "every upstream operation must run"
    );
    assert_eq!(
        task_129_unavailable, 0,
        "none of todo 129's nine session-mutating operations may remain backend-unavailable"
    );
    assert_eq!(
        unavailable, 10,
        "the explicit API gap inventory drifted; todo 132 removes its permission/question reply and reject routes"
    );
    assert_eq!(
        invoked.len() - unavailable,
        48,
        "backed operation count drifted; 58 upstream operations minus the 10 remaining 503 gaps"
    );
}

/// Success criterion 4's narrowing, frozen by NAME rather than by count.
///
/// The narrowing permits exactly these operations to answer `503
/// backend_unavailable`; everything else upstream declares must have a backend
/// whose status and normalized body are compared. A count alone cannot catch a
/// swap — one gap closing while another opens leaves the number at ten — so
/// the members are listed and
/// [`criterion_4_freezes_the_backend_unavailable_operations_by_name`] compares
/// this list against what the server *actually answers*, not against a constant
/// somewhere else.
///
/// Plan todo 132 implemented the permission/question `reply`/`reject` routes and
/// dropped this list to ten in the same commit.
const FROZEN_API_GAPS: &[(&str, &str)] = &[
    ("DELETE", "/api/credential/{credentialID}"),
    ("DELETE", "/api/integration/attempt/{attemptID}"),
    ("GET", "/api/integration/attempt/{attemptID}"),
    ("GET", "/api/session/{sessionID}/message/{messageID}"),
    ("GET", "/api/session/{sessionID}/permission/{requestID}"),
    ("PATCH", "/api/credential/{credentialID}"),
    ("POST", "/api/integration/attempt/{attemptID}/complete"),
    ("POST", "/api/integration/{integrationID}/connect/key"),
    ("POST", "/api/integration/{integrationID}/connect/oauth"),
    ("POST", "/api/session/{sessionID}/permission"),
];

#[tokio::test]
async fn criterion_4_freezes_the_backend_unavailable_operations_by_name() {
    let root = oc_testkit::subject::workspace_root().expect("workspace root");
    let document: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.join(ORACLE_OPENAPI_FIXTURE)).expect("read oracle OpenAPI"),
    )
    .expect("parse oracle OpenAPI");
    let matrix = api_behaviour_matrix(&document);
    let subject_root = tempfile::tempdir().expect("subject gap-freeze root");
    let subject = start_subject_server(subject_root.path()).await;

    let frozen: BTreeSet<(String, String)> = FROZEN_API_GAPS
        .iter()
        .map(|(method, path)| ((*method).to_owned(), (*path).to_owned()))
        .collect();
    assert_eq!(
        frozen.len(),
        FROZEN_API_GAPS.len(),
        "the frozen gap list contains a duplicate, which would let a real gap hide behind it"
    );

    let mut observed = BTreeSet::new();
    let mut closed_without_a_compared_backend = Vec::new();
    for row in &matrix {
        let operation = (row.method.to_ascii_uppercase(), row.path.clone());
        let status = observe_api_operation(subject.addr, row, subject_root.path())
            .await
            .status;
        if status == 503 {
            observed.insert(operation);
            continue;
        }
        // A member that leaves the frozen set has to arrive somewhere: an
        // operation answering 200 while its matrix row still says "exempt" is
        // reported as parity without ever being compared, which is the exact
        // laundering the narrowing forbids.
        if frozen.contains(&operation)
            && !(matches!(row.status, ApiDimension::Compared(_))
                && matches!(row.body, ApiDimension::Compared(_)))
        {
            closed_without_a_compared_backend.push(format!("{} {}", operation.0, operation.1));
        }
    }

    let appeared: Vec<_> = observed.difference(&frozen).cloned().collect();
    assert!(
        appeared.is_empty(),
        "{} operation(s) newly answer 503 backend_unavailable and are NOT in the frozen \
         criterion-4 gap set: {appeared:?}\n\nThe narrowed criterion 4 allows exactly {} named \
         gaps. A new one is a regression in coverage, not a member of the allow-list: give the \
         operation a backend, or get the plan owner to widen the criterion and add it here \
         deliberately.",
        appeared.len(),
        FROZEN_API_GAPS.len()
    );

    let departed: Vec<_> = frozen.difference(&observed).cloned().collect();
    assert!(
        departed.is_empty(),
        "{} frozen gap(s) no longer answer 503: {departed:?}\n\nThat is progress, but it must be \
         recorded: remove them from FROZEN_API_GAPS in the same commit that backs them, so the \
         set shrinking is an explicit edit. Todo 132 is expected to do exactly this for the \
         permission/question reply and reject routes.",
        departed.len()
    );

    assert!(
        closed_without_a_compared_backend.is_empty(),
        "{} operation(s) left the frozen gap set without gaining a compared backend: {}\n\nAn \
         operation that answers something other than 503 while its behaviour-matrix row exempts \
         both status and body is counted as neither a gap nor parity — it is invisible. Compare \
         it, or leave it as an explicit 503.",
        closed_without_a_compared_backend.len(),
        closed_without_a_compared_backend.join(", ")
    );

    eprintln!(
        "criterion 4: {} backend-unavailable operations frozen by name and observed exactly:\n{}",
        observed.len(),
        observed
            .iter()
            .map(|(method, path)| format!("  {method} {path}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[tokio::test]
async fn api_behaviour_matrix_compares_live_status_body_and_side_effects() {
    let Some(binary) = oracle_binary() else {
        eprintln!(
            "SKIPPED api_behaviour_matrix_compares_live_status_body_and_side_effects: {NO_ORACLE}"
        );
        return;
    };
    let oracle_root = tempfile::tempdir().expect("oracle matrix root");
    let oracle = start_oracle_server(&binary, oracle_root.path());
    warm_oracle(oracle.addr).await;

    let subject_root = tempfile::tempdir().expect("subject matrix root");
    let subject = start_subject_server(subject_root.path()).await;
    let subject_addr = subject.addr;
    let document: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            oc_testkit::subject::workspace_root()
                .expect("workspace root")
                .join(ORACLE_OPENAPI_FIXTURE),
        )
        .expect("read oracle OpenAPI"),
    )
    .expect("parse oracle OpenAPI");
    let matrix = api_behaviour_matrix(&document);
    let mut observed = BTreeSet::new();

    for path in ["/api/health", "/api/session", "/api/session/active"] {
        let oracle_response = raw_http(oracle.addr, "GET", path, None, false).await;
        let subject_response = raw_http(subject_addr, "GET", path, None, false).await;
        let oracle_observation = ApiObservation {
            status: oracle_response.0,
            normalized_body: serde_json::from_str(&oracle_response.1).unwrap_or_else(|error| {
                panic!("oracle {path} JSON: {error}: {}", oracle_response.1)
            }),
            side_effect: serde_json::Value::Null,
        };
        let subject_observation = ApiObservation {
            status: subject_response.0,
            normalized_body: serde_json::from_str(&subject_response.1).unwrap_or_else(|error| {
                panic!("subject {path} JSON: {error}: {}", subject_response.1)
            }),
            side_effect: serde_json::Value::Null,
        };
        compare_api_observation(
            &format!("GET {path}"),
            &oracle_observation,
            &subject_observation,
        )
        .unwrap_or_else(|error| panic!("{error}"));
        observed.insert((path.to_owned(), "get".to_owned()));
    }

    let oracle_global = raw_http(oracle.addr, "GET", "/api/event", None, true).await;
    let subject_global = raw_http(subject_addr, "GET", "/api/event", None, true).await;
    compare_api_observation(
        "GET /api/event",
        &ApiObservation {
            status: oracle_global.0,
            normalized_body: normalize_sse(&oracle_global.1),
            side_effect: serde_json::json!({"emitted": "server.connected"}),
        },
        &ApiObservation {
            status: subject_global.0,
            normalized_body: normalize_sse(&subject_global.1),
            side_effect: serde_json::json!({"emitted": "server.connected"}),
        },
    )
    .unwrap_or_else(|error| panic!("{error}"));
    observed.insert(("/api/event".to_owned(), "get".to_owned()));

    for row in &matrix {
        let key = (row.path.clone(), row.method.clone());
        if observed.contains(&key) || row.path == "/api/session/{sessionID}/event" {
            continue;
        }
        let oracle_observation = observe_api_operation(oracle.addr, row, oracle_root.path()).await;
        let subject_observation =
            observe_api_operation(subject_addr, row, subject_root.path()).await;
        assert_subject_operation_is_accounted(row, &subject_observation);
        compare_selected_api_dimensions(row, &oracle_observation, &subject_observation)
            .unwrap_or_else(|error| panic!("{error}"));
        if matches!(row.status, ApiDimension::Compared(_)) {
            // A row the matrix declares Compared must actually agree with the
            // released binary on status, normalized body and side effect. Reaching
            // this arm without comparing would be the accounting defect the Final
            // Wave reviewers rejected: a claim of parity backed by an invocation.
            compare_api_observation(
                &format!("{} {}", row.method.to_ascii_uppercase(), row.path),
                &oracle_observation,
                &subject_observation,
            )
            .unwrap_or_else(|error| panic!("{error}"));
            observed.insert(key);
            continue;
        }
        assert!(
            matches!(row.status, ApiDimension::Exempt(_))
                && matches!(row.body, ApiDimension::Exempt(_))
                && matches!(row.side_effect, ApiDimension::Exempt(_)),
            "a non-exact operation must carry a reason for every observed dimension"
        );
        eprintln!(
            "api-matrix observed {} {}: oracle_status={} subject_status={} oracle_changed={} subject_changed={}",
            row.method.to_ascii_uppercase(),
            row.path,
            oracle_observation.status,
            subject_observation.status,
            oracle_observation.side_effect["changed"],
            subject_observation.side_effect["changed"]
        );
        observed.insert(key);
    }

    let create = raw_http(
        oracle.addr,
        "POST",
        "/api/session",
        Some(r#"{"id":"ses_matrix"}"#),
        false,
    )
    .await;
    assert_eq!(create.0, 200, "oracle session fixture: {}", create.1);
    let switch = raw_http(
        oracle.addr,
        "POST",
        "/api/session/ses_matrix/agent",
        Some(r#"{"agent":"plan"}"#),
        false,
    )
    .await;
    assert_eq!(switch.0, 204, "oracle agent fixture: {}", switch.1);
    let oracle_session = raw_http(
        oracle.addr,
        "GET",
        "/api/session/ses_matrix/event?after=0",
        None,
        true,
    )
    .await;
    let subject_session = raw_http(
        subject_addr,
        "GET",
        "/api/session/ses_matrix/event?after=0",
        None,
        true,
    )
    .await;
    let oracle_event = normalize_sse(&oracle_session.1);
    let subject_event = normalize_sse(&subject_session.1);
    compare_api_observation(
        "GET /api/session/{sessionID}/event",
        &ApiObservation {
            status: oracle_session.0,
            side_effect: serde_json::json!({
                "event_type": oracle_event["type"],
                "sequence": oracle_event["durable"]["seq"]
            }),
            normalized_body: oracle_event,
        },
        &ApiObservation {
            status: subject_session.0,
            side_effect: serde_json::json!({
                "event_type": subject_event["type"],
                "sequence": subject_event["durable"]["seq"]
            }),
            normalized_body: subject_event,
        },
    )
    .unwrap_or_else(|error| panic!("{error}"));
    observed.insert((
        "/api/session/{sessionID}/event".to_owned(),
        "get".to_owned(),
    ));
    assert_eq!(
        observed.len(),
        UPSTREAM_API_OPERATIONS,
        "the live differential must observe every upstream operation"
    );
}

/// Refetch `/doc` from the pinned release and require the committed capture to be
/// exactly what it serves.
///
/// Every `/api` assertion in this suite reads the committed file. A capture taken
/// from one release and then compared against a differently-versioned binary is a
/// stale oracle that no other test in this file can detect — it would simply keep
/// agreeing with itself. This is the test that makes the recapture a fact rather
/// than a claim in a commit message, and it is why the `1.18.12` in the filename is
/// harmless: the bytes are re-derived from [`PINNED_RELEASE`] on every run.
#[tokio::test]
async fn the_committed_openapi_capture_is_what_the_pinned_release_serves() {
    let Some(binary) = oracle_binary() else {
        eprintln!(
            "SKIPPED the_committed_openapi_capture_is_what_the_pinned_release_serves: \
             {NO_ORACLE}; the committed capture was NOT re-derived from the pinned release"
        );
        return;
    };
    let root = tempfile::tempdir().expect("openapi recapture root");
    let server = start_oracle_server(&binary, root.path());
    let (status, served) = raw_http(server.addr, "GET", "/doc", None, false).await;
    assert_eq!(
        status, 200,
        "the release must serve its own OpenAPI document"
    );

    let fixture = oc_testkit::subject::workspace_root()
        .expect("workspace root")
        .join(ORACLE_OPENAPI_FIXTURE);
    let committed = std::fs::read_to_string(&fixture)
        .unwrap_or_else(|error| panic!("read {}: {error}", fixture.display()));

    assert_eq!(
        served.len(),
        committed.len(),
        "the committed capture is {} bytes but {} serves {} — recapture {} from the pinned \
         release and restate every count derived from it",
        committed.len(),
        PINNED_RELEASE,
        served.len(),
        ORACLE_OPENAPI_FIXTURE
    );
    assert!(
        served == committed,
        "the committed capture is not byte-identical to what {PINNED_RELEASE} serves at /doc, \
         even though both are {} bytes; the /api assertions in this file are reading a stale \
         oracle",
        committed.len()
    );

    let live: serde_json::Value = serde_json::from_str(&served).expect("the served /doc is JSON");
    assert_eq!(
        api_operations(&live).len(),
        UPSTREAM_API_OPERATIONS,
        "{PINNED_RELEASE} serves a different number of /api operations than \
         UPSTREAM_API_OPERATIONS declares; report the delta and update every count that \
         references it"
    );
    eprintln!(
        "openapi-recapture: {PINNED_RELEASE} served {} bytes at /doc, byte-identical to {}, \
         declaring {UPSTREAM_API_OPERATIONS} /api operations",
        served.len(),
        ORACLE_OPENAPI_FIXTURE
    );
}

#[test]
fn api_operations_are_a_superset_of_all_upstream_operations() {
    let root = oc_testkit::subject::workspace_root().expect("workspace root");
    let fixture = root.join(ORACLE_OPENAPI_FIXTURE);
    let text = std::fs::read_to_string(&fixture)
        .unwrap_or_else(|error| panic!("read {}: {error}", fixture.display()));
    let document: serde_json::Value = serde_json::from_str(&text).expect("parse oracle OpenAPI");

    let upstream = api_operations(&document);
    assert_eq!(
        upstream.len(),
        UPSTREAM_API_OPERATIONS,
        "the committed oracle capture no longer declares {UPSTREAM_API_OPERATIONS} /api operations"
    );

    let generated = oc_server::api::openapi();
    let served = api_operations(&generated);
    let missing: BTreeSet<_> = upstream.difference(&served).cloned().collect();
    assert!(
        missing.is_empty(),
        "every upstream /api operation must exist; missing {missing:?}"
    );

    let extra: BTreeSet<_> = served.difference(&upstream).cloned().collect();
    let declared_c8: BTreeSet<(String, String)> = [
        ("/api/session/prune".to_owned(), "get".to_owned()),
        ("/api/session/prune".to_owned(), "post".to_owned()),
    ]
    .into();
    assert_eq!(
        extra, declared_c8,
        "operations served beyond upstream must be exactly the C8 maintenance endpoints declared \
         in docs/divergences.toml under c8-maintenance-endpoints"
    );

    eprintln!(
        "api-operations: upstream={} served={} missing={} added={}",
        upstream.len(),
        served.len(),
        missing.len(),
        extra.len()
    );
}

// ---------------------------------------------------------------------------
// The allow-list is data the gate consults
// ---------------------------------------------------------------------------

#[test]
fn the_divergence_allow_list_declares_exactly_the_expected_entries() {
    let list = DivergenceList::load().expect("docs/divergences.toml must load");
    assert_eq!(
        list.len(),
        divergence::DECLARED_COUNT,
        "{} declares {} divergence(s) but oc_testkit::divergence::DECLARED_COUNT expects {}. \
         Adding a divergence requires an entry AND a bump; this assertion exists so neither can \
         happen alone. Declared ids: {:?}",
        list.path().display(),
        list.len(),
        divergence::DECLARED_COUNT,
        list.ids()
    );
    for entry in list.entries() {
        assert!(!entry.id.trim().is_empty(), "an entry has no id");
        assert!(
            !entry.surface.trim().is_empty(),
            "divergence {} has no surface",
            entry.id
        );
        assert!(
            !entry.reason.trim().is_empty(),
            "divergence {} has no reason",
            entry.id
        );
    }

    let declared: BTreeSet<&str> = list.ids().into_iter().collect();
    let mut undeclared = Vec::new();
    for difference in behavioural_differences() {
        if !declared.contains(difference.declared_as.as_str()) {
            undeclared.push(format!(
                "  {} (surface: {})\n    must be declared as {:?}, which {} does not \
                 contain\n    upstream: {}\n    asserted by: {}",
                difference.id,
                difference.surface,
                difference.declared_as,
                list.path().display(),
                difference.upstream_evidence,
                difference.asserted_by
            ));
        }
    }
    assert!(
        undeclared.is_empty(),
        "{} behavioural difference(s) from upstream are NOT declared in {}:\n{}\n\n\
         The allow-list is the single place a reader consults for behavioural \
         differences. Until plan todo 119 this assertion ran the other way — it \
         required each of these to stay OUT of the file — which made the omission \
         criterion 17 forbids into something no gate could ever fail. Declare the \
         difference (and bump `divergence::DECLARED_COUNT` in the same commit), or \
         merge it into the entry that already covers it by pointing `declared_as` at \
         that entry. Do not delete the record to make this pass.\n\
         declared ids: {:?}",
        undeclared.len(),
        list.path().display(),
        undeclared.join("\n"),
        list.ids()
    );
    eprintln!(
        "divergences: {} declared: {:?}; {} behavioural difference(s) each resolved to a \
         declared entry",
        list.len(),
        list.ids(),
        behavioural_differences().len()
    );
}

#[test]
fn the_execute_tools_live_schema_matches_its_divergence_entry() {
    let list = DivergenceList::load().expect("docs/divergences.toml must load");
    let entry = list
        .find(divergence::EXECUTE_CONTRACT_ID)
        .unwrap_or_else(|| {
            panic!(
                "{} must declare {:?}; the plan requires this divergence to be verified, not \
                 merely mentioned",
                list.path().display(),
                divergence::EXECUTE_CONTRACT_ID
            )
        });
    let contract = entry
        .contract
        .as_ref()
        .expect("the execute entry must carry a [divergence.contract] table");

    let schema = oc_tool::schema::params_schema::<oc_tools::ExecuteParams>();
    let properties: Vec<String> = schema["properties"]
        .as_object()
        .expect("the execute schema must be an object schema")
        .keys()
        .cloned()
        .collect();
    let mut required: Vec<String> = schema["required"]
        .as_array()
        .expect("the execute schema must declare required properties")
        .iter()
        .map(|value| {
            value
                .as_str()
                .expect("a required entry must be a string")
                .to_owned()
        })
        .collect();
    required.sort();

    assert_eq!(
        properties, contract.properties,
        "the `execute` tool's live top-level properties no longer match what \
         docs/divergences.toml declares. The model-facing contract changed; update the entry in \
         the same commit or revert the schema change."
    );
    assert_eq!(
        required, contract.required,
        "the `execute` tool's live required properties no longer match its divergence entry"
    );

    let subcall = oc_tool::schema::derive_params_schema::<oc_tools::batch::Subcall>();
    let subcall_properties: BTreeSet<String> = subcall["properties"]
        .as_object()
        .expect("the sub-call schema must be an object schema")
        .keys()
        .cloned()
        .collect();
    let declared: BTreeSet<String> = contract.subcall_properties.iter().cloned().collect();
    assert!(
        declared.is_subset(&subcall_properties),
        "the divergence entry declares sub-call control properties the live schema does not have: \
         {:?}",
        declared.difference(&subcall_properties).collect::<Vec<_>>()
    );

    assert_eq!(
        contract.upstream_properties,
        ["code"],
        "upstream's contract is `{{ code: string }}` at \
         packages/opencode/src/tool/code-mode.ts:12-20; the entry must keep saying so"
    );
    assert!(
        !properties.contains(&"code".to_owned()),
        "if `execute` grew a `code` parameter the divergence would no longer exist and the entry \
         must be removed"
    );
    eprintln!(
        "execute-parameter-contract: upstream={:?} live={:?} required={:?}",
        contract.upstream_properties, properties, required
    );
}

// ---------------------------------------------------------------------------
// The registry cannot describe comparisons that no longer exist
// ---------------------------------------------------------------------------

#[test]
fn every_registered_evidence_test_still_exists() {
    let root = oc_testkit::subject::workspace_root().expect("workspace root");
    let mut checked = 0usize;
    for surface in SURFACES {
        if surface.evidence == "(none)" {
            assert_eq!(
                surface.verdict,
                Verdict::NotCompared,
                "surface {} claims no evidence but is not marked not_compared",
                surface.id
            );
            continue;
        }
        let (file, test_name) = surface.evidence.split_once("::").unwrap_or_else(|| {
            panic!("surface {}: evidence must be `path::test_name`", surface.id)
        });
        let path = root.join(file);
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "surface {} names evidence in {} which cannot be read: {error}",
                surface.id,
                path.display()
            )
        });
        assert!(
            text.contains(&format!("fn {test_name}(")),
            "surface {} names test `{test_name}` in {}, which no longer defines it. Either the \
             comparison was renamed (update this row) or it was deleted (the claim must go with \
             it).",
            surface.id,
            path.display()
        );
        checked += 1;
    }
    assert!(
        checked >= 15,
        "only {checked} evidence tests were resolved; the registry is looking in the wrong place \
         and would pass vacuously"
    );
    eprintln!(
        "surface registry: {checked} evidence tests resolved of {} rows",
        SURFACES.len()
    );
}

/// The behavioural assertion each declared difference names must exist and run.
///
/// A declared divergence whose behaviour has silently reverted is worse than an
/// undeclared one: the allow-list then states, with a reason and an upstream
/// citation, something the binary no longer does. So the named test must both be
/// defined and be collected — `#[ignore]` is checked because it is the way a test
/// stops running while its name stays exactly where a search would find it.
#[test]
fn every_declared_behavioural_difference_names_a_test_that_exists_and_runs() {
    let root = oc_testkit::subject::workspace_root().expect("workspace root");
    let differences = behavioural_differences();
    for difference in &differences {
        let (file, test_name) = difference.asserted_by.split_once("::").unwrap_or_else(|| {
            panic!(
                "behavioural difference {}: asserted_by must be `path::test_name`, got {:?}",
                difference.id, difference.asserted_by
            )
        });
        let path = root.join(file);
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "behavioural difference {} names its assertion in {} which cannot be read: \
                 {error}",
                difference.id,
                path.display()
            )
        });
        let signature = format!("fn {test_name}(");
        let offset = text.find(&signature).unwrap_or_else(|| {
            panic!(
                "behavioural difference {} names test `{test_name}` in {}, which no longer \
                 defines it. The allow-list entry {:?} would then declare a behaviour nothing \
                 proves. Either the assertion was renamed (update this row) or the behaviour \
                 changed (the declaration must change with it).",
                difference.id,
                path.display(),
                difference.declared_as
            )
        });
        let attributes = &text[..offset];
        let recent = attributes
            .rfind("\n\n")
            .map_or(attributes, |blank| &attributes[blank..]);
        assert!(
            !recent.contains("#[ignore"),
            "behavioural difference {} names test `{test_name}` in {}, which is `#[ignore]`d. An \
             ignored test keeps the name a search finds while proving nothing, so the declared \
             difference would be unverified.",
            difference.id,
            path.display()
        );
    }
    assert!(
        differences.len() >= 6,
        "only {} behavioural difference(s) were checked; the six reconciled by plan todo 119 are \
         the floor, and a shrinking list is how a difference stops being reported",
        differences.len()
    );
    eprintln!(
        "behavioural differences: {} checked, each naming a live assertion",
        differences.len()
    );
}

#[test]
fn every_surface_id_is_unique_and_every_verdict_is_explained() {
    let mut seen = BTreeSet::new();
    for surface in SURFACES {
        assert!(
            seen.insert(surface.id),
            "duplicate surface id {}",
            surface.id
        );
        assert!(
            !surface.detail.trim().is_empty(),
            "surface {} has no detail; an unexplained verdict is indistinguishable from an \
             oversight",
            surface.id
        );
        if surface.verdict == Verdict::NotCompared {
            assert_eq!(
                surface.oracle,
                OracleKind::None,
                "surface {} is not compared but names an oracle",
                surface.id
            );
            assert!(
                surface.detail.contains("NOT COMPARED"),
                "surface {}: an uncompared surface must say so in its detail, in words a reader \
                 skimming the report cannot miss",
                surface.id
            );
        }
    }
    let allow_list = DivergenceList::load().expect("docs/divergences.toml must load");
    for difference in behavioural_differences() {
        assert!(
            !difference.upstream_evidence.trim().is_empty(),
            "behavioural difference {} cites no upstream source; without one it is a claim, not a \
             difference",
            difference.id
        );
        assert!(
            !difference.asserted_by.trim().is_empty(),
            "behavioural difference {} names no test, so nothing proves the behaviour is live",
            difference.id
        );
        let entry = allow_list
            .find(difference.declared_as.as_str())
            .unwrap_or_else(|| {
                panic!(
                    "behavioural difference {} points at allow-list entry {:?}, which {} does \
                     not declare",
                    difference.id,
                    difference.declared_as,
                    allow_list.path().display()
                )
            });
        assert!(
            !entry.reason.trim().is_empty(),
            "the entry {:?} declaring behavioural difference {} carries no reason",
            difference.declared_as,
            difference.id
        );
    }
    for normalization in normalizations() {
        assert!(
            !normalization.reason.trim().is_empty(),
            "the normalization of {:?} on {} carries no reason; an unjustified mask is how a \
             suite is made green by hiding a real difference",
            normalization.value,
            normalization.surface
        );
    }
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

#[test]
fn the_suite_emits_a_machine_readable_report() {
    let resolved = resolved_oracle();
    let list = DivergenceList::load().expect("docs/divergences.toml must load");

    let surfaces = SURFACES
        .iter()
        .map(|row| {
            let verdict = match (row.verdict, row.oracle, resolved.is_some()) {
                (Verdict::NotCompared, _, _) => Verdict::NotCompared,
                (verdict, OracleKind::LiveBinary, false) => {
                    eprintln!(
                        "SKIPPED surface {}: {NO_ORACLE}; recorded as skipped rather than compared",
                        row.id
                    );
                    let _ = verdict;
                    Verdict::Skipped
                }
                (verdict, _, _) => verdict,
            };
            ComparedSurface {
                id: row.id.to_owned(),
                name: row.name.to_owned(),
                verdict,
                oracle: row.oracle,
                evidence: row.evidence.to_owned(),
                detail: row.detail.to_owned(),
                measured: Vec::new(),
            }
        })
        .collect();

    let report = CompatReport {
        schema_version: SCHEMA_VERSION,
        generated_by: "cargo test -p oc-testkit --test compat_suite",
        oracle: OracleAvailability {
            available: resolved.is_some(),
            version: resolved.map(|oracle| oracle.version.clone()),
            path: resolved.map(|oracle| oracle.program.clone()),
            pinned_source_version: PINNED_RELEASE.to_owned(),
        },
        divergences: DivergenceSummary {
            declared_count: list.len(),
            expected_count: divergence::DECLARED_COUNT,
            ids: list.ids().into_iter().map(str::to_owned).collect(),
        },
        surfaces,
        normalizations: normalizations(),
        behavioural_differences: behavioural_differences(),
        known_gaps: known_gaps(),
    };

    assert!(
        !report.with_verdict(Verdict::Compared).is_empty()
            || !report.with_verdict(Verdict::Skipped).is_empty(),
        "a report claiming nothing was compared and nothing was skipped is a bug in the suite"
    );

    // The artifact must not claim a build that did not run. `version` is what the
    // resolved binary printed for `--version`; `pinned_source_version` is what the
    // artifact tells its readers the comparison was made against. F1's finding B1 was
    // that these two disagreed — 1.18.13 recorded, 1.18.12 executed — and no gate
    // could fail over it.
    if let Some(probed) = report.oracle.version.as_deref() {
        assert_eq!(
            probed,
            report.oracle.pinned_source_version,
            "the report records pinned_source_version={} while the binary it measured against, \
             {}, reports {probed}. A reader cannot tell which upstream build the compatibility \
             claim was measured against, which is what F1 rejected.",
            report.oracle.pinned_source_version,
            report
                .oracle
                .path
                .as_deref()
                .unwrap_or(Path::new("<unknown>"))
                .display()
        );
    } else {
        assert!(
            !report.oracle.available,
            "the report claims an available oracle but recorded no version for it"
        );
        eprintln!(
            "SKIPPED the recorded-version agreement check: {NO_ORACLE}; the report records \
             pinned_source_version={} with no measured version to compare it to",
            report.oracle.pinned_source_version
        );
    }

    let destination = CompatReport::destination().expect("resolve the report destination");
    report.write(&destination).expect("write the report");
    eprintln!("{}", report.summary());
    eprintln!("compatibility report written to {}", destination.display());

    let written = std::fs::read_to_string(&destination).expect("read the report back");
    for row in SURFACES {
        assert!(
            written.contains(row.id),
            "the report omits surface {}",
            row.id
        );
    }
}

/// Every behavioural difference from upstream, each bound to its allow-list entry.
///
/// # What this replaced, and why the inversion matters
///
/// This was `nominated_divergences()`: six deliberate differences recorded here
/// *because* they were not in `docs/divergences.toml`, with
/// [`the_divergence_allow_list_is_loadable_and_counted`] asserting each one stayed
/// **outside** the file so the plan's declared count of eight kept holding. Two
/// structures then reported the same kind of fact, and the direction of the
/// assertion made the omission permanent: the only way to fail was to *declare* a
/// difference. F1 and F4 both rejected on it.
///
/// Every record now names the entry in the allow-list that must cover it, the
/// upstream evidence that makes it a difference rather than a guess, and the test
/// that proves the behaviour is live. The gate resolves `declared_as` against the
/// loaded file, so deleting or renaming an entry fails here — which is the
/// assertion the previous shape could not express.
///
/// Six nominations became four entries. `subpath-matches-literally` and
/// `memory-subsystem` were not independent differences, so they share a
/// `declared_as` with the difference they belong to instead of being declared
/// twice; both merges are recorded in the allow-list's own header with their
/// upstream evidence.
fn behavioural_differences() -> Vec<BehaviouralDifference> {
    vec![
        BehaviouralDifference {
            id: "subpath-is-implemented".to_owned(),
            surface: "GET /api/session?project=…&subpath=…; oc-db session listing".to_owned(),
            declared_as: "session-subpath-is-applied".to_owned(),
            upstream_evidence: "declared in packages/core/src/session.ts:68-76, packages/protocol/src/groups/session.ts:98-110 and the generated SDK; forwarded by packages/server/src/handlers/session.ts:23-37; never read by the query in packages/core/src/session.ts:268-277".to_owned(),
            asserted_by: "crates/oc-db/tests/session.rs::the_project_scope_with_a_subpath_actually_filters".to_owned(),
        },
        BehaviouralDifference {
            id: "subpath-matches-literally".to_owned(),
            surface: "GET /api/session?project=…&subpath=…".to_owned(),
            declared_as: "session-subpath-is-applied".to_owned(),
            upstream_evidence: "the un-escaped `like(SessionTable.path, sql.param(`${input.path}/%`))` is on the legacy /session?path= handler at packages/opencode/src/session/session.ts:969-980, a route this port does not serve; the v2 surface ignores subpath entirely".to_owned(),
            asserted_by: "crates/oc-db/tests/session.rs::a_subpath_containing_a_like_wildcard_is_not_treated_as_a_pattern".to_owned(),
        },
        BehaviouralDifference {
            id: "context-md-excluded".to_owned(),
            surface: "project instruction cascade".to_owned(),
            declared_as: "context-md-excluded".to_owned(),
            upstream_evidence: "packages/opencode/src/session/instruction.ts:60-68 lists CONTEXT.md in `instructionFiles`; :122-132 probes it through findUp; :155-168 reads and injects every resolved path".to_owned(),
            asserted_by: "crates/oc-config/tests/instructions.rs::context_md_is_never_loaded".to_owned(),
        },
        BehaviouralDifference {
            id: "malformed-auth-json-is-an-error".to_owned(),
            surface: "$XDG_DATA_HOME/opencode/auth.json".to_owned(),
            declared_as: "malformed-auth-json-is-an-error".to_owned(),
            upstream_evidence: "packages/opencode/src/auth/index.ts:58-67 maps any read or parse failure to `{}` via orElseSucceed; :73-80 then writes `{ ...data, [norm]: info }` over the file".to_owned(),
            asserted_by: "crates/oc-auth/src/store.rs::malformed_json_is_a_typed_error_naming_the_file".to_owned(),
        },
        BehaviouralDifference {
            id: "failed-format-restores-pre-format-bytes".to_owned(),
            surface: "post-edit formatter execution".to_owned(),
            declared_as: "failed-format-restores-pre-format-bytes".to_owned(),
            upstream_evidence: "packages/opencode/src/format/index.ts:73-114 checks `result.exitCode !== 0` and only logs; a spawn failure is mapped to undefined; nothing is snapshotted or written back".to_owned(),
            asserted_by: "crates/oc-tools/tests/format.rs::a_formatter_that_truncates_the_file_before_failing_has_its_damage_undone".to_owned(),
        },
        BehaviouralDifference {
            id: "non-pure-plugin-generated-trees".to_owned(),
            surface: "`debug config` without OPENCODE_PURE — the plugin-generated `agent` and `command` trees".to_owned(),
            declared_as: divergence::NON_PURE_PLUGIN_TREES_ID.to_owned(),
            upstream_evidence: format!(
                "measured on the user's real /config/.config/opencode/opencode.json in .omo/evidence/F1-REPORT-wave2.md: the released binary's own plugin set synthesises a {}-byte `agent` tree and a {}-byte `command` tree that this port leaves empty, so success criterion 2's byte-identical comparison was narrowed to pure mode",
                divergence::NON_PURE_AGENT_TREE_BYTES, divergence::NON_PURE_COMMAND_TREE_BYTES
            ),
            asserted_by: "crates/oc-config/tests/differential.rs::criterion_2_is_narrowed_to_pure_mode_and_the_non_pure_plugin_trees_are_declared".to_owned(),
        },
        BehaviouralDifference {
            id: "memory-subsystem".to_owned(),
            surface: "system prompt, `memory` tool, reflection fork".to_owned(),
            declared_as: "cross-session-resident-memory".to_owned(),
            upstream_evidence: "upstream opencode 1.18.13 has no cross-session memory subsystem at all, so no upstream behaviour to compare against".to_owned(),
            asserted_by: "crates/oc-memory/tests/integration.rs::memory_false_matches_a_real_upstream_control_and_spawns_no_reflection".to_owned(),
        },
    ]
}

/// The gap list, rendered from the live gate's own counts.
///
/// The entries moved to [`oc_testkit::compat_report::known_gaps`] so that the
/// committed compatibility matrix can render the same list this report carries;
/// before that they existed only inside `target/compat/compat-report.json`, which
/// nothing commits, while `docs/divergences.md` told readers twice that a gap is
/// "listed in the compatibility matrix".
fn known_gaps() -> Vec<KnownGap> {
    let v1 = oc_server::compat_v1::v1_coverage();
    compat_report::known_gaps(
        FROZEN_API_GAPS.len(),
        UPSTREAM_API_OPERATIONS,
        compat_report::V1SurfaceCoverage::new(v1.measured, v1.served, v1.redirected),
    )
}

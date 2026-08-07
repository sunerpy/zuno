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
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use oc_testkit::compat_report::{DivergenceSummary, OracleAvailability, OracleKind};
use oc_testkit::{
    ComparedSurface, CompatReport, DivergenceList, KnownGap, NominatedDivergence, Normalization,
    Verdict, compat_report::SCHEMA_VERSION, divergence,
};

/// The installed release the whole port is measured against.
///
/// Hard-coded, not discovered, for the same reason `oc-db/tests/schema.rs` does
/// it: a differential against "whatever `opencode` is on `PATH`" is a differential
/// against an unknown, and the version gap is then unreportable.
const ORACLE_BINARY: &str = "/config/.local/share/mise/installs/opencode/1.18.12/opencode";

/// The version this port reports to the npm plugin compatibility gate.
const PINNED_SOURCE_VERSION: &str = "1.18.13";

/// The committed capture of the real binary's OpenAPI document.
const ORACLE_OPENAPI_FIXTURE: &str = ".omo/fixtures/oracle-openapi-1.18.12.json";

/// Upstream `/api` operations this port does not serve.
///
/// Both are the SSE event streams. This port serves an equivalent stream at the
/// compat path `/event` (`crates/oc-server/src/events/route.rs:20`) but registers
/// nothing under `/api/`, so the plan's "every upstream path exists here" is not
/// yet true. Asserted as an exact set so a *third* absence fails rather than
/// widening the exemption, and reported as a [`KnownGap`] rather than entered in
/// `docs/divergences.toml` — an omission is not a decision.
const API_KNOWN_GAPS: [(&str, &str); 2] = [
    ("/api/event", "get"),
    ("/api/session/{sessionID}/event", "get"),
];

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
        id: "api-operations",
        name: "/api path+method set and served OpenAPI document",
        verdict: Verdict::PartiallyCompared,
        oracle: OracleKind::CommittedFixture,
        evidence: "crates/oc-testkit/tests/compat_suite.rs::api_operations_are_a_superset_of_upstream_minus_the_two_known_gaps",
        detail: "56 of 58 upstream operations served; 2 SSE streams absent under /api (see known_gaps); 2 C8 operations added (declared divergence c8-maintenance-endpoints)",
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
            value: "OpenAPI schema bodies, descriptions, and component ordering".to_owned(),
            reason: "the comparison is over the path+method SET, which is the contract a client binds to. Response shapes are compared per group by crates/oc-server/tests/api.rs, not here; claiming a document-level byte match would overstate what this target checks.".to_owned(),
        },
    ]
}

// ---------------------------------------------------------------------------
// Oracle plumbing
// ---------------------------------------------------------------------------

fn oracle_binary() -> Option<PathBuf> {
    std::env::var_os("OPENCODE_TEST_BINARY")
        .map(PathBuf::from)
        .or_else(|| {
            Path::new(ORACLE_BINARY)
                .is_file()
                .then(|| ORACLE_BINARY.into())
        })
}

fn oracle_version(binary: &Path) -> Option<String> {
    let output = Command::new(binary).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

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
            "SKIPPED db_schema_matches_a_database_the_real_binary_created: no opencode binary at \
             {ORACLE_BINARY}; the SQLite schema was NOT compared"
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
            "SKIPPED journal_round_trip_through_the_real_binary_does_not_replay_migrations: no \
             opencode binary at {ORACLE_BINARY}; the journal round-trip was NOT run"
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

#[test]
fn api_operations_are_a_superset_of_upstream_minus_the_two_known_gaps() {
    let root = oc_testkit::subject::workspace_root().expect("workspace root");
    let fixture = root.join(ORACLE_OPENAPI_FIXTURE);
    let text = std::fs::read_to_string(&fixture)
        .unwrap_or_else(|error| panic!("read {}: {error}", fixture.display()));
    let document: serde_json::Value = serde_json::from_str(&text).expect("parse oracle OpenAPI");

    let upstream = api_operations(&document);
    assert_eq!(
        upstream.len(),
        58,
        "the committed oracle capture no longer declares 58 /api operations"
    );

    let generated = oc_server::api::openapi();
    let served = api_operations(&generated);
    let gaps: BTreeSet<(String, String)> = API_KNOWN_GAPS
        .iter()
        .map(|(path, method)| ((*path).to_owned(), (*method).to_owned()))
        .collect();

    let missing: BTreeSet<_> = upstream.difference(&served).cloned().collect();
    assert_eq!(
        missing, gaps,
        "the set of upstream /api operations this port does not serve changed; a new absence must \
         be either implemented or added to API_KNOWN_GAPS with a reason, never silently tolerated"
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
    for nominated in nominated_divergences() {
        assert!(
            !declared.contains(nominated.id.as_str()),
            "{} is both declared in {} and listed as a nomination outside the plan's count; it \
             must be one or the other",
            nominated.id,
            list.path().display()
        );
    }
    eprintln!(
        "divergences: {} declared: {:?}; {} further declared in code but outside the plan's count",
        list.len(),
        list.ids(),
        nominated_divergences().len()
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
    for nominated in nominated_divergences() {
        assert!(
            !nominated.declared_at.trim().is_empty(),
            "nominated divergence {} cites no source; a nomination without one is a claim, not a \
             record",
            nominated.id
        );
        assert!(
            !nominated.reason.trim().is_empty(),
            "nominated divergence {} has no reason",
            nominated.id
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
    let binary = oracle_binary();
    let list = DivergenceList::load().expect("docs/divergences.toml must load");

    let surfaces = SURFACES
        .iter()
        .map(|row| {
            let verdict = match (row.verdict, row.oracle, binary.is_some()) {
                (Verdict::NotCompared, _, _) => Verdict::NotCompared,
                (verdict, OracleKind::LiveBinary, false) => {
                    eprintln!(
                        "SKIPPED surface {}: no opencode binary at {ORACLE_BINARY}; recorded as \
                         skipped rather than compared",
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
            available: binary.is_some(),
            version: binary.as_deref().and_then(oracle_version),
            path: binary,
            pinned_source_version: PINNED_SOURCE_VERSION.to_owned(),
        },
        divergences: DivergenceSummary {
            declared_count: list.len(),
            expected_count: divergence::DECLARED_COUNT,
            ids: list.ids().into_iter().map(str::to_owned).collect(),
        },
        surfaces,
        normalizations: normalizations(),
        nominated_divergences: nominated_divergences(),
        known_gaps: known_gaps(),
    };

    assert!(
        !report.with_verdict(Verdict::Compared).is_empty()
            || !report.with_verdict(Verdict::Skipped).is_empty(),
        "a report claiming nothing was compared and nothing was skipped is a bug in the suite"
    );

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

/// Deliberate differences declared in code that the plan's seven does not cover.
///
/// The plan's count is asserted, so these cannot be added to
/// `docs/divergences.toml` without contradicting it. They are reported instead,
/// each citing where it is already declared, so the omission is data rather than
/// something a reader has to notice. `subpath` is the sharpest case: todo 21's
/// decision record nominates it for this very allow-list, by name.
fn nominated_divergences() -> Vec<NominatedDivergence> {
    vec![
        NominatedDivergence {
            id: "subpath-is-implemented".to_owned(),
            surface: "GET /api/session?project=…&subpath=…; oc-db session listing".to_owned(),
            reason: "Upstream declares `subpath` in the union, the HTTP schema, the generated client and the SDK, then never reads it in the handler; this port applies it, which changes results for a request upstream silently ignores.".to_owned(),
            declared_at: ".omo/notepads/opencode-rust/decisions.md:1969-2008 (\"DIVERGENCE CANDIDATE #1 … for Todo 86's allow-list\")".to_owned(),
        },
        NominatedDivergence {
            id: "subpath-matches-literally".to_owned(),
            surface: "GET /api/session?project=…&subpath=…".to_owned(),
            reason: "Matched as a literal tree prefix rather than a SQL LIKE pattern, so a path containing `_` or `%` cannot act as a wildcard the way upstream's v1 interpolation allows.".to_owned(),
            declared_at: ".omo/notepads/opencode-rust/decisions.md:1990-1999 (\"a second, smaller divergence … should go on the allow-list with the first\")".to_owned(),
        },
        NominatedDivergence {
            id: "context-md-excluded".to_owned(),
            surface: "project instruction cascade".to_owned(),
            reason: "`CONTEXT.md` is deprecated upstream and is not read here, so a repository whose only instruction file is CONTEXT.md loads zero project instructions under this binary and one under the TypeScript binary.".to_owned(),
            declared_at: ".omo/notepads/opencode-rust/decisions.md:925-939".to_owned(),
        },
        NominatedDivergence {
            id: "malformed-auth-json-is-an-error".to_owned(),
            surface: "$XDG_DATA_HOME/opencode/auth.json".to_owned(),
            reason: "Upstream funnels a read failure into an empty store, so the next write destroys every credential in a truncated file; this port returns an error instead.".to_owned(),
            declared_at: ".omo/notepads/opencode-rust/decisions.md:1524-1537".to_owned(),
        },
        NominatedDivergence {
            id: "failed-format-restores-pre-format-bytes".to_owned(),
            surface: "post-edit formatter execution".to_owned(),
            reason: "Upstream keeps whatever a failing formatter left on disk; this port restores the bytes the edit wrote, at the price of discarding useful partial work from a formatter that exits non-zero after doing some.".to_owned(),
            declared_at: ".omo/notepads/opencode-rust/decisions.md:4075-4090".to_owned(),
        },
        NominatedDivergence {
            id: "memory-subsystem".to_owned(),
            surface: "system prompt, `memory` tool, reflection fork".to_owned(),
            reason: "Upstream has no memory subsystem at all, so nothing here is justifiable as compatibility; plan todo 103 is the todo required to add this entry and bump the asserted count to eight.".to_owned(),
            declared_at: ".omo/plans/opencode-rust.md:1017-1020; .omo/notepads/opencode-rust/learnings.md:1188".to_owned(),
        },
    ]
}

fn known_gaps() -> Vec<KnownGap> {
    vec![
        KnownGap {
            id: "api-event-streams".to_owned(),
            surface: "HTTP GET /api/event, GET /api/session/{sessionID}/event".to_owned(),
            detail: "Not served under /api/. An equivalent SSE stream exists at the compat path \
                     /event (crates/oc-server/src/events/route.rs:20), so the capability is \
                     present but the upstream paths are not. This is a GAP, not a divergence: \
                     the plan's success criterion 4 requires upstream's operation set to be a \
                     subset of this port's, and today it is not."
                .to_owned(),
        },
        KnownGap {
            id: "permission-evaluation-semantics".to_owned(),
            surface: "permission resolution (`findLast` wildcard matching)".to_owned(),
            detail: "The merged permission CONFIG is compared against the real binary; the \
                     evaluation order that turns it into an allow/ask/deny decision is verified \
                     against the upstream source by unit tests, not differentially, because the \
                     binary exposes no command that prints a resolved decision."
                .to_owned(),
        },
        KnownGap {
            id: "channel-dependent-database-filename".to_owned(),
            surface: "$XDG_DATA_HOME/opencode/opencode-<channel>.db".to_owned(),
            detail: "A source build of either implementation resolves opencode-local.db while an \
                     installed release resolves opencode.db, so a `cargo build` does not see the \
                     user's sessions. This port mirrors the oracle's rule \
                     (packages/core/src/database/database.ts:45-55) exactly, so it is FAITHFUL \
                     BEHAVIOUR and not a divergence — recorded here because it presents as a \
                     parity bug the first time anyone tries it. Plan todo 92 owns documenting it."
                .to_owned(),
        },
    ]
}

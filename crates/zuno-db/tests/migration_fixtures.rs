//! Upgrades from databases that released Zuno versions actually wrote.
//!
//! Reconstructing an "old" database by removing pieces of the current schema
//! cannot notice a column an old release never had, or an object an old release
//! wrote that the current schema no longer describes. The fixtures under
//! `tests/fixtures/` close that gap: each one is the
//! DDL a tagged release's `migration::create_current` executed, recovered from that
//! tag's `schema.rs` and `migration/mod.rs` (the header of every file names the
//! exact `git show` commands), followed by representative rows.
//!
//! | fixture        | format | release | what the upgrade must add                          |
//! |----------------|--------|---------|----------------------------------------------------|
//! | `format-5.sql` | 5      | v0.0.3  | learning flywheel, Plan stack, verification ledger |
//! | `format-6.sql` | 6      | v0.2.2  | Plan stack, verification ledger                    |
//! | `format-7.sql` | 7      | v0.6.7  | verification ledger                                |
//! | `format-8.sql` | 8      | v0.10.5 | session memory policy                              |
//! | `format-9.sql` | 9      | v0.10.21| execution control and completion routing            |
//! | `format-10.sql`| 10     | v0.10.23| versioned memory, provenance, leases and search     |
//! | `format-11.sql`| 11     | v0.10.28| automatic memory provenance and maintenance        |
//! | `format-12.sql`| 12     | v0.10.29| question metadata and command receipt ledger       |
//! | `format-13.sql`| 13     | v0.10.30| input receipts, context usage, goal-resume purpose |
//!
//! Every fixture is upgraded through the real entry point, [`migration::apply`],
//! and the result is compared *structurally* with a database `apply` creates from
//! nothing: table set, per-table columns (name, declared type, nullability,
//! default, primary-key position), foreign keys, and indexes (table, uniqueness,
//! partiality, indexed columns, including SQLite's automatic constraint indexes).
//! `sqlite_master.sql` text is deliberately not compared: the format-6 step uses
//! `ALTER TABLE ... ADD COLUMN`, which yields the same structure with different
//! text and appends the new columns after the old ones, so columns are matched by
//! name rather than `cid`. [`upgraded_work_plan_columns_are_appended_not_interleaved`]
//! pins that one known divergence.
//!
//! The atomicity tests make the upgrade fail on its final DDL statement, after
//! every earlier statement in the same transaction has run, and prove with
//! SQLite's own statement trace that the marker update never executed and that the
//! marker, the rows, and the whole object inventory are exactly what the fixture
//! loaded.
//!
//! Design reference: Codex eaa8b6d917,
//! `codex-rs/state/src/migrations_tests.rs::thread_attachment_migration_preserves_existing_data`.
//! Zuno adapts the seeded-old-data comparison to frozen released fixtures and
//! marker-last rollback. Codex's `runtime_migrator` future-version tolerance is
//! deliberately not adopted: Zuno rejects future and unmarked formats.

use rusqlite::OptionalExtension as _;
use rusqlite::trace::{TraceEvent, TraceEventCodes};
use rusqlite::types::Value;
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Barrier, Mutex, PoisonError};
use zuno_db::{Connection, migration, open};
use zuno_error::DbError;
use zuno_paths::DbLocation;
use zuno_types::question::{QuestionItem, QuestionOrigin, QuestionRequest};

// ---------------------------------------------------------------------------
// Fixtures and the steps that separate them from the current format.
// ---------------------------------------------------------------------------

/// One checked-in database as an old release wrote it.
struct Fixture {
    format: u32,
    release: &'static str,
    sql: &'static str,
    /// Application tables plus `zuno_schema`, as counted in `sqlite_master`.
    table_count: usize,
}

const FORMAT_FIVE: Fixture = Fixture {
    format: 5,
    release: "v0.0.3",
    sql: include_str!("fixtures/format-5.sql"),
    table_count: 27,
};

const FORMAT_SIX: Fixture = Fixture {
    format: 6,
    release: "v0.2.2",
    sql: include_str!("fixtures/format-6.sql"),
    table_count: 37,
};

const FORMAT_SEVEN: Fixture = Fixture {
    format: 7,
    release: "v0.6.7",
    sql: include_str!("fixtures/format-7.sql"),
    table_count: 38,
};

const FORMAT_EIGHT_SQL: &str = concat!(
    include_str!("fixtures/format-7.sql"),
    include_str!("fixtures/format-8.sql")
);

const FORMAT_EIGHT: Fixture = Fixture {
    format: 8,
    release: "v0.10.5",
    sql: FORMAT_EIGHT_SQL,
    table_count: 39,
};

const FORMAT_NINE_SQL: &str = concat!(
    include_str!("fixtures/format-7.sql"),
    include_str!("fixtures/format-8.sql"),
    include_str!("fixtures/format-9.sql")
);

const FORMAT_NINE: Fixture = Fixture {
    format: 9,
    release: "v0.10.21",
    sql: FORMAT_NINE_SQL,
    table_count: 40,
};

const FORMAT_TEN_SQL: &str = concat!(
    include_str!("fixtures/format-7.sql"),
    include_str!("fixtures/format-8.sql"),
    include_str!("fixtures/format-9.sql"),
    include_str!("fixtures/format-10.sql")
);

const FORMAT_TEN: Fixture = Fixture {
    format: 10,
    release: "v0.10.23",
    sql: FORMAT_TEN_SQL,
    table_count: 42,
};

/// Every table `sqlite_master` lists once the current schema is in place.
const CURRENT_TABLE_COUNT: usize = 67;

const FORMAT_ELEVEN: Fixture = Fixture {
    format: 11,
    release: "v0.10.28",
    sql: concat!(
        include_str!("fixtures/format-7.sql"),
        include_str!("fixtures/format-8.sql"),
        include_str!("fixtures/format-9.sql"),
        include_str!("fixtures/format-10.sql"),
        include_str!("fixtures/format-11.sql")
    ),
    table_count: 55,
};

const FORMAT_TWELVE: Fixture = Fixture {
    format: 12,
    release: "v0.10.29",
    sql: concat!(
        include_str!("fixtures/format-7.sql"),
        include_str!("fixtures/format-8.sql"),
        include_str!("fixtures/format-9.sql"),
        include_str!("fixtures/format-10.sql"),
        include_str!("fixtures/format-11.sql"),
        include_str!("fixtures/format-12.sql")
    ),
    table_count: 57,
};

const FORMAT_THIRTEEN: Fixture = Fixture {
    format: 13,
    release: "v0.10.30",
    sql: concat!(
        include_str!("fixtures/format-7.sql"),
        include_str!("fixtures/format-8.sql"),
        include_str!("fixtures/format-9.sql"),
        include_str!("fixtures/format-10.sql"),
        include_str!("fixtures/format-11.sql"),
        include_str!("fixtures/format-12.sql"),
        include_str!("fixtures/format-13.sql")
    ),
    table_count: 59,
};

const SUPPORTED_FIXTURES: &[&Fixture] = &[
    &FORMAT_FIVE,
    &FORMAT_SIX,
    &FORMAT_SEVEN,
    &FORMAT_EIGHT,
    &FORMAT_NINE,
    &FORMAT_TEN,
    &FORMAT_ELEVEN,
    &FORMAT_TWELVE,
    &FORMAT_THIRTEEN,
];

/// One additive upgrade step, described by what it must leave behind and by the
/// first statement `schema.rs` runs for it (used to prove, from the statement
/// trace, that the step executed before a later failure rolled it back).
struct Step {
    name: &'static str,
    first_statement: &'static str,
    tables: &'static [&'static str],
    indexes: &'static [&'static str],
    columns: &'static [(&'static str, &'static str)],
}

const LEARNING: Step = Step {
    name: "learning flywheel (format 5 -> 6)",
    first_statement: "CREATE TABLE `message_feedback`",
    tables: &[
        "message_feedback",
        "learning_job",
        "experience_record",
        "experience_evidence",
        "learning_pattern",
        "evaluation_suite",
        "evaluation_case",
        "evaluation_run",
        "evaluation_result",
        "skill_candidate",
    ],
    indexes: &[
        "message_feedback_session_updated_idx",
        "learning_job_status_scheduled_idx",
        "learning_job_project_kind_status_idx",
        "learning_job_extraction_source_idx",
        "experience_record_extraction_ordinal_idx",
        "experience_record_project_status_time_idx",
        "experience_record_session_time_idx",
        "experience_record_fingerprint_idx",
        "experience_evidence_identity_idx",
        "experience_evidence_experience_idx",
        "learning_pattern_scope_fingerprint_idx",
        "learning_pattern_scope_status_updated_idx",
        "evaluation_case_suite_name_idx",
        "evaluation_run_candidate_time_idx",
        "evaluation_result_run_case_idx",
        "skill_candidate_project_status_time_idx",
        "skill_candidate_pattern_idx",
        "skill_candidate_pattern_digest_unique_idx",
    ],
    columns: &[],
};

const PLAN_STACK: Step = Step {
    name: "Plan stack (format 6 -> 7)",
    first_statement: "ALTER TABLE work_plan ADD COLUMN parent_plan_id",
    tables: &["work_plan_archive"],
    indexes: &["work_plan_archive_session_state_idx"],
    columns: &[
        ("work_plan", "parent_plan_id"),
        ("work_plan", "stack_depth"),
    ],
};

const VERIFICATION: Step = Step {
    name: "verification ledger (format 7 -> 8)",
    first_statement: "CREATE TABLE `verification_receipt`",
    tables: &["verification_receipt"],
    indexes: &[
        "verification_receipt_call_idx",
        "verification_receipt_session_time_idx",
    ],
    columns: &[],
};

const MEMORY_POLICY: Step = Step {
    name: "session memory policy (format 8 -> 9)",
    first_statement: "CREATE TABLE `session_memory_policy`",
    tables: &["session_memory_policy"],
    indexes: &["session_memory_policy_generation_updated_idx"],
    columns: &[],
};

const EXECUTION: Step = Step {
    name: "session execution state (format 9 -> 10)",
    first_statement: "ALTER TABLE `session_input` ADD COLUMN `source_key`",
    tables: &["session_execution_state", "completion_delivery"],
    indexes: &[
        "session_input_session_source_key_idx",
        "session_execution_state_mode_phase_updated_idx",
        "completion_delivery_session_owner_updated_idx",
    ],
    columns: &[
        ("session_input", "source_key"),
        ("session_input", "trigger_kind"),
        ("session_input", "cycle_id"),
    ],
};

const MEMORY_RUNTIME: Step = Step {
    name: "memory runtime (format 10 -> 11)",
    first_statement: "CREATE TABLE `resident_memory_document`",
    tables: &[
        "resident_memory_document",
        "resident_memory_revision",
        "learning_retrieval_snapshot",
        "experience_search_fts",
        "experience_search_fts_config",
        "experience_search_fts_data",
        "experience_search_fts_docsize",
        "experience_search_fts_idx",
        "experience_search_cjk_fts",
        "experience_search_cjk_fts_config",
        "experience_search_cjk_fts_data",
        "experience_search_cjk_fts_docsize",
        "experience_search_cjk_fts_idx",
    ],
    indexes: &[
        "resident_memory_revision_candidate_idx",
        "message_session_user_boundary_idx",
        "experience_record_usage_idx",
    ],
    columns: &[
        ("experience_record", "evidence_verified"),
        ("experience_record", "last_used_at"),
        ("experience_record", "use_count"),
        ("experience_evidence", "source_digest"),
        ("experience_evidence", "verified"),
        ("experience_evidence", "promotion_eligible"),
        ("learning_job", "lease_token"),
    ],
};

/// The index created by the final DDL statement of every upgrade path. Index names
/// are database-global, so an unrelated index that already owns this name makes
/// exactly that statement fail after everything before it ran inside the same
/// transaction. SQLite rejects the duplicate while *preparing* the statement, so
/// it never reaches `SQLITE_TRACE_STMT`; the statement immediately before it is
/// therefore the last one the trace can show before the rollback.
const TRAP_INDEX: &str = "runtime_attempt_worker_state_idx";
const STATEMENT_BEFORE_TRAP: &str = "CREATE INDEX runtime_job_ready_idx";

const AUTOMATIC_MEMORY: Step = Step {
    name: "automatic memory (format 11 -> 12)",
    first_statement: "ALTER TABLE memory_candidate ADD COLUMN base_revision",
    tables: &["resident_memory_provenance", "memory_maintenance_state"],
    indexes: &[
        "memory_candidate_path_status_updated_idx",
        "resident_memory_provenance_candidate_idx",
    ],
    columns: &[
        ("memory_candidate", "base_revision"),
        ("memory_candidate", "evidence"),
    ],
};

const QUESTIONS: Step = Step {
    name: "question companions and scheduling (format 12 -> 13)",
    first_statement: "CREATE TABLE question_interaction",
    tables: &["question_interaction", "question_action_receipt"],
    indexes: &["question_interaction_purpose_authorization_idx"],
    columns: &[("session_execution_state", "scheduling")],
};

const RUNTIME_CONSISTENCY: Step = Step {
    name: "input receipts, context usage, and goal-resume purpose (format 13 -> 14)",
    first_statement: "CREATE TABLE question_interaction_v14",
    tables: &["session_input_receipt", "session_context_usage"],
    indexes: &[
        "session_input_receipt_turn_state_idx",
        "session_context_usage_updated_idx",
    ],
    columns: &[],
};

const PREVIEW_OVERLAY: Step = Step {
    name: "preview ownership and runtime overlay",
    first_statement: "CREATE TABLE session_ownership",
    tables: &[
        "session_ownership",
        "runtime_session",
        "runtime_job",
        "runtime_attempt",
        "runtime_owner_schedule",
        "zuno_preview_schema",
    ],
    indexes: &[
        "session_ownership_principal_idx",
        "runtime_session_lease_deadline_idx",
        "runtime_job_ready_idx",
        "runtime_attempt_worker_state_idx",
    ],
    columns: &[],
};

fn steps_after(format: u32) -> &'static [&'static Step] {
    match format {
        5 => &[
            &LEARNING,
            &PLAN_STACK,
            &VERIFICATION,
            &MEMORY_POLICY,
            &EXECUTION,
            &MEMORY_RUNTIME,
            &AUTOMATIC_MEMORY,
            &QUESTIONS,
            &RUNTIME_CONSISTENCY,
            &PREVIEW_OVERLAY,
        ],
        6 => &[
            &PLAN_STACK,
            &VERIFICATION,
            &MEMORY_POLICY,
            &EXECUTION,
            &MEMORY_RUNTIME,
            &AUTOMATIC_MEMORY,
            &QUESTIONS,
            &RUNTIME_CONSISTENCY,
            &PREVIEW_OVERLAY,
        ],
        7 => &[
            &VERIFICATION,
            &MEMORY_POLICY,
            &EXECUTION,
            &MEMORY_RUNTIME,
            &AUTOMATIC_MEMORY,
            &QUESTIONS,
            &RUNTIME_CONSISTENCY,
            &PREVIEW_OVERLAY,
        ],
        8 => &[
            &MEMORY_POLICY,
            &EXECUTION,
            &MEMORY_RUNTIME,
            &AUTOMATIC_MEMORY,
            &QUESTIONS,
            &RUNTIME_CONSISTENCY,
            &PREVIEW_OVERLAY,
        ],
        9 => &[
            &EXECUTION,
            &MEMORY_RUNTIME,
            &AUTOMATIC_MEMORY,
            &QUESTIONS,
            &RUNTIME_CONSISTENCY,
            &PREVIEW_OVERLAY,
        ],
        10 => &[
            &MEMORY_RUNTIME,
            &AUTOMATIC_MEMORY,
            &QUESTIONS,
            &RUNTIME_CONSISTENCY,
            &PREVIEW_OVERLAY,
        ],
        11 => &[
            &AUTOMATIC_MEMORY,
            &QUESTIONS,
            &RUNTIME_CONSISTENCY,
            &PREVIEW_OVERLAY,
        ],
        12 => &[&QUESTIONS, &RUNTIME_CONSISTENCY, &PREVIEW_OVERLAY],
        13 => &[&RUNTIME_CONSISTENCY, &PREVIEW_OVERLAY],
        other => panic!("no fixture describes format {other}"),
    }
}

#[test]
fn published_fixture_matrix_covers_every_supported_format_five_through_thirteen() {
    assert_eq!(migration::CURRENT_FORMAT, 14);
    assert_eq!(
        SUPPORTED_FIXTURES
            .iter()
            .map(|fixture| fixture.format)
            .collect::<Vec<_>>(),
        (5..=13).collect::<Vec<_>>()
    );
}

#[test]
fn format_thirteen_fixture_is_the_v0_10_30_database_with_frozen_ddl() {
    assert_fixture_is_the_old_format(&FORMAT_THIRTEEN);
    let fixture = include_str!("fixtures/format-13.sql");
    for (section, digest) in [
        (
            "questions.sql",
            "c15bc8e71e662618646653915b15aa46dda371c5e3c4f5b435835a83d25353c4",
        ),
        (
            "scheduling.sql",
            "2e0541aac322b475128873def272500a298b1b4c24c2b56af8556ad486e61808",
        ),
    ] {
        let start = format!("-- BEGIN v0.10.30 {section}\n");
        let end = format!("-- END v0.10.30 {section}\n");
        let (_, published) = fixture.split_once(&start).expect("frozen DDL start");
        let (published, _) = published.split_once(&end).expect("frozen DDL end");
        assert_eq!(
            hex::encode(Sha256::digest(published.as_bytes())),
            digest,
            "{section} must retain the exact v0.10.30 bytes"
        );
    }
}

#[test]
fn format_thirteen_upgrade_preserves_every_row_and_reaches_the_current_structure() {
    assert_upgrade_preserves_rows_and_reaches_the_current_structure(&FORMAT_THIRTEEN);
}

#[test]
fn format_thirteen_failed_upgrade_preserves_the_original_database() {
    assert_failed_upgrade_leaves_the_database_untouched(&FORMAT_THIRTEEN);
}

#[test]
fn format_twelve_fixture_is_the_v0_10_29_database_with_frozen_ddl() {
    assert_fixture_is_the_old_format(&FORMAT_TWELVE);
    let fixture = include_str!("fixtures/format-12.sql");
    let (_, published) = fixture
        .split_once("-- BEGIN v0.10.29 automatic_memory.sql\n")
        .expect("the published DDL starts here");
    let (published, _) = published
        .split_once("-- END v0.10.29 automatic_memory.sql\n")
        .expect("the published DDL ends here");
    assert_eq!(
        hex::encode(Sha256::digest(published.as_bytes())),
        "6732ae158d4be5bffc9dcf37bd78f5efd6cc2608cfe2d02b30defd66c6709026",
        "the fixture must retain the exact released automatic-memory DDL and both indexes"
    );
}

#[test]
fn format_twelve_upgrade_preserves_sessions_messages_memory_and_human_requests() {
    assert_upgrade_preserves_rows_and_reaches_the_current_structure(&FORMAT_TWELVE);
}

#[test]
fn format_twelve_failed_upgrade_preserves_the_original_database() {
    assert_failed_upgrade_leaves_the_database_untouched(&FORMAT_TWELVE);
}

#[test]
fn format_eleven_fixture_matches_the_released_schema() {
    assert_fixture_is_the_old_format(&FORMAT_ELEVEN);
    let fixture = include_str!("fixtures/format-11.sql");
    let ddl = fixture.split("CREATE TABLE").nth(1).expect("DDL start");
    assert!(ddl.starts_with(" `resident_memory_document`"));
    // The checked-in delta stays byte-identical to the released DDL, not a schema
    // assembled by removing features from the new implementation.
    assert!(fixture.contains(include_str!("../src/schema/memory_runtime.sql").trim()));
}

#[test]
fn format_eleven_upgrade_preserves_resident_revisions_and_reaches_current_structure() {
    assert_upgrade_preserves_rows_and_reaches_the_current_structure(&FORMAT_ELEVEN);
}

#[test]
fn format_eleven_failed_upgrade_preserves_original_columns_rows_and_marker() {
    assert_failed_upgrade_leaves_the_database_untouched(&FORMAT_ELEVEN);
}

#[test]
fn format_eleven_backfills_automatic_source_links_without_reclassifying_user_reaffirmations() {
    for reaffirmed in [false, true] {
        let dir = temp_dir();
        let mut connection = load_fixture(&dir.path().join("zuno.db"), &FORMAT_ELEVEN);
        connection.execute_batch(
            "INSERT INTO memory_candidate(id,target,target_path,action,content,reason,confidence,
                source_kind,status,before_entries,after_entries,time_created,time_updated,time_applied)
             VALUES('legacy-auto','project','legacy-path-alias','add','Keep reviewed resident memory.',
                'Verified source',9900,'reflection','applied','[]','[\"Keep reviewed resident memory.\"]',
                10,10,10);
             UPDATE experience_record SET promoted_memory_candidate_id='legacy-auto' WHERE id='exp_fixture_0001';
             UPDATE resident_memory_revision SET operation='apply',candidate_id='legacy-auto' WHERE revision=2;"
        ).expect("released automatic memory with aliased path");
        if reaffirmed {
            connection.execute_batch(
                "INSERT INTO memory_candidate(id,target,target_path,action,content,reason,confidence,
                    source_kind,status,before_entries,after_entries,time_created,time_updated,time_applied)
                 VALUES('user-reaffirmation','project','C:/Users/0791/project/.zuno/RULES.md','add',
                    'Keep reviewed resident memory.','User confirmation',10000,'user','applied',
                    '[\"Keep reviewed resident memory.\"]','[\"Keep reviewed resident memory.\"]',20,20,20);"
            ).expect("newer user affirmation");
        }
        migration::apply(&mut connection).expect("upgrade");
        let count: i64 = connection
            .query_row(
                "SELECT count(*) FROM resident_memory_provenance",
                [],
                |row| row.get(0),
            )
            .expect("source links");
        assert_eq!(count, i64::from(!reaffirmed));
        let content: String = connection
            .query_row("SELECT entries FROM resident_memory_document", [], |row| {
                row.get(0)
            })
            .expect("preserved memory");
        assert_eq!(content, "[\"Keep reviewed resident memory.\"]");
        assert_eq!(
            connection
                .query_row(
                    "SELECT status FROM memory_candidate WHERE id='legacy-auto'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .expect("history"),
            "applied"
        );
    }
}

#[test]
fn format_ten_fixture_matches_the_released_schema() {
    assert_fixture_is_the_old_format(&FORMAT_TEN);
}

#[test]
fn format_ten_upgrade_preserves_rows_and_reaches_current_structure() {
    assert_upgrade_preserves_rows_and_reaches_the_current_structure(&FORMAT_TEN);
}

#[test]
fn format_ten_failed_upgrade_preserves_the_original_database() {
    assert_failed_upgrade_leaves_the_database_untouched(&FORMAT_TEN);
}

/// Values every fixture carries, checked as literals so the byte-for-byte claim is
/// anchored to what the file says rather than to whatever happened to load.
const REPRESENTATIVE_VALUES: &[(&str, &str)] = &[
    (
        "SELECT title FROM session WHERE id = 'ses_fixture_0001'",
        "Migrate the ledger — 迁移账本",
    ),
    (
        "SELECT data FROM message WHERE id = 'msg_fixture_0001'",
        r#"{"id":"msg_fixture_0001","role":"user","sessionID":"ses_fixture_0001","time":{"created":1735689700200}}"#,
    ),
    (
        "SELECT json_extract(data, '$.text') FROM part WHERE id = 'prt_fixture_0001'",
        "Keep every row — 保留每一行 — and add nothing silently.\n\tTabs and \"quotes\" survive too.",
    ),
    (
        "SELECT content FROM memory_candidate WHERE id = 'mem_fixture_0001'",
        "Run `cargo test -p zuno-db` before every release.",
    ),
    (
        "SELECT steps FROM work_plan WHERE session_id = 'ses_fixture_0001'",
        r#"[{"id":"inspect","title":"Inspect the old ledger","status":"completed"},{"id":"upgrade","title":"Upgrade in one transaction","status":"in_progress"}]"#,
    ),
    (
        "SELECT data FROM session_message WHERE id = 'sem_fixture_0001'",
        r#"{"kind":"prompt.admitted","inputID":"inp_fixture_0001","digest":"sha256:9f2c"}"#,
    ),
];

/// Values only the formats with the learning flywheel carry.
const LEARNING_REPRESENTATIVE_VALUES: &[(&str, &str)] = &[
    (
        "SELECT summary FROM experience_record WHERE id = 'exp_fixture_0001'",
        "The user wants `cargo test -p zuno-db` to run before every release.",
    ),
    (
        "SELECT note FROM message_feedback WHERE message_id = 'msg_fixture_0001'",
        "Exactly the right level of caution.",
    ),
];

const VERIFICATION_REPRESENTATIVE_VALUES: &[(&str, &str)] = &[(
    "SELECT summary FROM verification_receipt WHERE id = 'vrc_fixture_0001'",
    "The format-8 verification receipt survives.",
)];

const MEMORY_POLICY_REPRESENTATIVE_VALUES: &[(&str, &str)] = &[(
    "SELECT reason FROM session_memory_policy WHERE session_id = 'ses_fixture_0001'",
    "format-9 policy survives",
)];

// ---------------------------------------------------------------------------
// Structural inventory: what a database *is*, independent of how its DDL was spelled.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct Structure {
    format: Option<u32>,
    tables: BTreeMap<String, TableShape>,
    indexes: BTreeMap<String, IndexShape>,
    /// Views, triggers, and anything else `sqlite_master` lists; none is expected.
    other_objects: BTreeSet<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TableShape {
    columns: BTreeMap<String, ColumnShape>,
    foreign_keys: BTreeSet<ForeignKeyShape>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ColumnShape {
    declared_type: String,
    not_null: bool,
    default: Option<String>,
    primary_key: i64,
}

/// One `pragma_foreign_key_list` row: `(id, seq, table, from, to, on_update, on_delete)`.
type ForeignKeyRow = (i64, i64, String, String, Option<String>, String, String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ForeignKeyShape {
    from: Vec<String>,
    table: String,
    to: Vec<Option<String>>,
    on_update: String,
    on_delete: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexShape {
    table: String,
    unique: bool,
    /// `c` for `CREATE INDEX`, `u` for a `UNIQUE` constraint, `pk` for a primary key.
    origin: String,
    partial: bool,
    /// `None` marks an expression column such as `json_extract(...)`.
    columns: Vec<Option<String>>,
}

fn structure(connection: &Connection) -> Structure {
    let mut tables = BTreeMap::new();
    let mut other_objects = BTreeSet::new();
    let mut statement = connection
        .prepare(
            "SELECT type, name FROM sqlite_master \
             WHERE name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .expect("prepare the sqlite_master inventory");
    let objects: Vec<(String, String)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query sqlite_master")
        .collect::<Result<_, _>>()
        .expect("collect sqlite_master");
    for (kind, name) in objects {
        match kind.as_str() {
            "table" => {
                tables.insert(name.clone(), table_shape(connection, &name));
            }
            // Indexes are gathered per table below so automatic constraint indexes count too.
            "index" => {}
            _ => {
                other_objects.insert((kind, name));
            }
        }
    }
    let mut indexes = BTreeMap::new();
    for table in tables.keys() {
        for (name, shape) in index_shapes(connection, table) {
            assert!(
                indexes.insert(name.clone(), shape).is_none(),
                "index {name} listed twice"
            );
        }
    }
    let format = if tables.contains_key("zuno_schema") {
        connection
            .query_row(
                "SELECT format FROM zuno_schema WHERE singleton = 1",
                [],
                |row| row.get::<_, u32>(0),
            )
            .optional()
            .expect("read the format marker")
    } else {
        None
    };
    Structure {
        format,
        tables,
        indexes,
        other_objects,
    }
}

fn table_shape(connection: &Connection, table: &str) -> TableShape {
    let mut statement = connection
        .prepare("SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info(?1)")
        .expect("prepare pragma_table_info");
    let columns = statement
        .query_map([table], |row| {
            Ok((
                row.get::<_, String>(0)?,
                ColumnShape {
                    declared_type: row.get(1)?,
                    not_null: row.get::<_, i64>(2)? != 0,
                    default: row.get(3)?,
                    primary_key: row.get(4)?,
                },
            ))
        })
        .expect("query pragma_table_info")
        .collect::<Result<BTreeMap<_, _>, _>>()
        .expect("collect columns");

    let mut statement = connection
        .prepare(
            "SELECT id, seq, \"table\", \"from\", \"to\", on_update, on_delete \
             FROM pragma_foreign_key_list(?1) ORDER BY id, seq",
        )
        .expect("prepare pragma_foreign_key_list");
    let rows: Vec<ForeignKeyRow> = statement
        .query_map([table], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })
        .expect("query pragma_foreign_key_list")
        .collect::<Result<_, _>>()
        .expect("collect foreign keys");
    let mut grouped: BTreeMap<i64, ForeignKeyShape> = BTreeMap::new();
    for (id, _seq, target, from, to, on_update, on_delete) in rows {
        let entry = grouped.entry(id).or_insert_with(|| ForeignKeyShape {
            from: Vec::new(),
            table: target,
            to: Vec::new(),
            on_update,
            on_delete,
        });
        entry.from.push(from);
        entry.to.push(to);
    }
    TableShape {
        columns,
        foreign_keys: grouped.into_values().collect(),
    }
}

fn index_shapes(connection: &Connection, table: &str) -> Vec<(String, IndexShape)> {
    let mut statement = connection
        .prepare("SELECT name, \"unique\", origin, partial FROM pragma_index_list(?1)")
        .expect("prepare pragma_index_list");
    let listed: Vec<(String, bool, String, bool)> = statement
        .query_map([table], |row| {
            Ok((
                row.get(0)?,
                row.get::<_, i64>(1)? != 0,
                row.get(2)?,
                row.get::<_, i64>(3)? != 0,
            ))
        })
        .expect("query pragma_index_list")
        .collect::<Result<_, _>>()
        .expect("collect indexes");
    listed
        .into_iter()
        .map(|(name, unique, origin, partial)| {
            let mut statement = connection
                .prepare("SELECT name FROM pragma_index_info(?1) ORDER BY seqno")
                .expect("prepare pragma_index_info");
            let columns = statement
                .query_map([&name], |row| row.get::<_, Option<String>>(0))
                .expect("query pragma_index_info")
                .collect::<Result<_, _>>()
                .expect("collect index columns");
            (
                name,
                IndexShape {
                    table: table.to_owned(),
                    unique,
                    origin,
                    partial,
                    columns,
                },
            )
        })
        .collect()
}

/// Compare two inventories and report every difference by name, because the
/// `Debug` output of a whole [`Structure`] is too large to read in a failure.
fn assert_same_structure(actual: &Structure, expected: &Structure, context: &str) {
    let mut differences = Vec::new();
    if actual.format != expected.format {
        differences.push(format!(
            "format marker: {:?} vs {:?}",
            actual.format, expected.format
        ));
    }
    for name in actual
        .tables
        .keys()
        .chain(expected.tables.keys())
        .collect::<BTreeSet<_>>()
    {
        match (actual.tables.get(name), expected.tables.get(name)) {
            (Some(left), Some(right)) if left != right => {
                for column in left
                    .columns
                    .keys()
                    .chain(right.columns.keys())
                    .collect::<BTreeSet<_>>()
                {
                    if left.columns.get(column) != right.columns.get(column) {
                        differences.push(format!(
                            "table {name} column {column}: {:?} vs {:?}",
                            left.columns.get(column),
                            right.columns.get(column)
                        ));
                    }
                }
                if left.foreign_keys != right.foreign_keys {
                    differences.push(format!(
                        "table {name} foreign keys: {:?} vs {:?}",
                        left.foreign_keys, right.foreign_keys
                    ));
                }
            }
            (Some(_), None) => differences.push(format!("table {name}: only in actual")),
            (None, Some(_)) => differences.push(format!("table {name}: only in expected")),
            _ => {}
        }
    }
    for name in actual
        .indexes
        .keys()
        .chain(expected.indexes.keys())
        .collect::<BTreeSet<_>>()
    {
        if actual.indexes.get(name) != expected.indexes.get(name) {
            differences.push(format!(
                "index {name}: {:?} vs {:?}",
                actual.indexes.get(name),
                expected.indexes.get(name)
            ));
        }
    }
    if actual.other_objects != expected.other_objects {
        differences.push(format!(
            "other objects: {:?} vs {:?}",
            actual.other_objects, expected.other_objects
        ));
    }
    assert!(
        differences.is_empty(),
        "{context}: structures differ\n  {}",
        differences.join("\n  ")
    );
    // The field-by-field walk above is the readable report; this is the guarantee.
    assert_eq!(actual, expected, "{context}");
}

// ---------------------------------------------------------------------------
// Row snapshots: every value in every table, projected onto the columns that
// existed when the snapshot was taken.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct TableRows {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
}

fn snapshot_rows(connection: &Connection, structure: &Structure) -> BTreeMap<String, TableRows> {
    structure
        .tables
        .iter()
        .map(|(table, shape)| {
            let columns: Vec<String> = shape.columns.keys().cloned().collect();
            let rows = read_rows(connection, table, &columns);
            (table.clone(), TableRows { columns, rows })
        })
        .collect()
}

fn read_rows(connection: &Connection, table: &str, columns: &[String]) -> Vec<Vec<Value>> {
    let projection = columns
        .iter()
        .map(|column| format!("`{column}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut statement = connection
        .prepare(&format!(
            "SELECT {projection} FROM `{table}` ORDER BY {projection}"
        ))
        .expect("prepare the row snapshot");
    statement
        .query_map([], |row| {
            (0..columns.len())
                .map(|index| row.get::<_, Value>(index))
                .collect::<Result<Vec<_>, _>>()
        })
        .expect("query the row snapshot")
        .collect::<Result<_, _>>()
        .expect("collect the row snapshot")
}

/// Every row of every table in `before` still reads back identically, except in
/// the tables named by `allowed_to_change` (a successful upgrade rewrites exactly
/// one row: the `zuno_schema` marker, which is asserted on its own). Columns an
/// upgrade added are outside the projection and are asserted separately.
fn assert_rows_preserved(
    connection: &Connection,
    before: &BTreeMap<String, TableRows>,
    allowed_to_change: &[&str],
) {
    for (table, expected) in before {
        if allowed_to_change.contains(&table.as_str()) {
            continue;
        }
        let actual = read_rows(connection, table, &expected.columns);
        assert_eq!(
            actual, expected.rows,
            "rows of `{table}` changed across the upgrade (columns {:?})",
            expected.columns
        );
    }
}

fn assert_literal_values(connection: &Connection, expectations: &[(&str, &str)], context: &str) {
    for (query, expected) in expectations {
        let actual: String = connection
            .query_row(query, [], |row| row.get(0))
            .unwrap_or_else(|error| panic!("{context}: `{query}` failed: {error}"));
        assert_eq!(actual, *expected, "{context}: `{query}`");
    }
}

// ---------------------------------------------------------------------------
// Opening helpers.
// ---------------------------------------------------------------------------

fn temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("create a temporary directory")
}

/// Open a fresh file database through this crate and load `fixture` into it, exactly
/// as if the named release had left the file behind.
fn load_fixture(path: &Path, fixture: &Fixture) -> Connection {
    let connection = open::open_at(path).expect("open a fresh database file");
    connection
        .execute_batch(fixture.sql)
        .unwrap_or_else(|error| panic!("load the {} fixture: {error}", fixture.release));
    if fixture.format < 13 {
        connection
            .execute_batch(include_str!("fixtures/legacy-human-requests.sql"))
            .expect("seed released human-request payloads and responses");
    }
    // Seed data through columns shared by the published layouts. The frozen DDL
    // files stay untouched; the matrix must exercise a nonempty receipt backfill.
    for (ordinal, state) in [
        "queued",
        "steering",
        "promoted",
        "consumed",
        "cancelled",
        "failed",
    ]
    .into_iter()
    .enumerate()
    {
        let sequence = i64::try_from(ordinal).expect("bounded fixture ordinal") + 100;
        connection
            .execute(
                "INSERT INTO session_input
             (id,session_id,prompt,delivery,state,revision,admitted_seq,promoted_seq,error,
              time_created,time_updated)
             VALUES (?1,'ses_fixture_0001',?2,?3,?4,2,?5,?6,?7,1735690000000,1735690001000)",
                rusqlite::params![
                    format!("inp_migration_{state}"),
                    format!("Preserve {state} input — 保留输入"),
                    if state == "steering" {
                        "steer"
                    } else {
                        "queue"
                    },
                    state,
                    sequence,
                    matches!(state, "promoted" | "consumed").then_some(sequence + 100),
                    (state == "failed").then_some("historical failure — 原始错误"),
                ],
            )
            .expect("seed historical inbox states");
    }
    connection
}

/// The reference: what `migration::apply` builds when nothing exists yet.
fn fresh_current() -> Connection {
    let mut connection = open::open(&DbLocation::Memory).expect("open an in-memory database");
    migration::apply(&mut connection).expect("create the current schema");
    connection
}

fn column_order(connection: &Connection, table: &str) -> Vec<String> {
    let mut statement = connection
        .prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")
        .expect("prepare pragma_table_info");
    statement
        .query_map([table], |row| row.get(0))
        .expect("query pragma_table_info")
        .collect::<Result<_, _>>()
        .expect("collect column order")
}

/// Legacy question text becomes typed companion metadata; its original ledger
/// remains authoritative for state and answers, including every terminal state.
fn assert_question_backfill(connection: &Connection) {
    let mut statement = connection
        .prepare(
            "SELECT h.id,h.payload,h.message_id,h.call_id,q.purpose,q.mode,q.definition,
                    q.decision,q.authorization,q.risk_reason,q.authorization_input_id
             FROM human_request h LEFT JOIN question_interaction q ON q.request_id=h.id
             WHERE h.kind='input' AND json_extract(h.payload,'$.source')='question'
             ORDER BY h.id",
        )
        .expect("prepare legacy question companions");
    let mut rows = statement
        .query([])
        .expect("query legacy question companions");
    let mut count = 0;
    while let Some(row) = rows.next().expect("read legacy companion") {
        let id: String = row.get(0).expect("request ID");
        let payload: String = row.get(1).expect("original payload");
        let payload: serde_json::Value = serde_json::from_str(&payload).expect("legacy JSON");
        let message_id: Option<String> = row.get(2).expect("message ID");
        let call_id: Option<String> = row.get(3).expect("call ID");
        assert_eq!(
            row.get::<_, String>(4).expect("backfilled purpose"),
            "clarification",
            "{id}"
        );
        assert_eq!(
            row.get::<_, String>(5).expect("backfilled mode"),
            "blocking",
            "{id}"
        );
        let definition: String = row.get(6).expect("backfilled definition");
        let definition: serde_json::Value =
            serde_json::from_str(&definition).expect("definition JSON");
        let origin: QuestionOrigin =
            serde_json::from_value(definition["origin"].clone()).expect("typed origin");
        assert_eq!(origin.session_id, "ses_fixture_0001", "{id}");
        assert_eq!(origin.message_id, message_id, "{id}");
        assert_eq!(origin.call_id, call_id, "{id}");
        assert_eq!(origin.goal_id, None, "{id}");
        assert_eq!(origin.turn_id, None, "{id}");
        let questions: Vec<QuestionItem> =
            serde_json::from_value(definition["questions"].clone()).expect("typed questions");
        let legacy: Vec<QuestionRequest> =
            serde_json::from_value(payload["questions"].clone()).expect("legacy questions");
        assert_eq!(
            questions
                .iter()
                .map(|item| &item.question)
                .collect::<Vec<_>>(),
            legacy.iter().collect::<Vec<_>>(),
            "{id}: original questions survive intact"
        );
        let ids: BTreeSet<&str> = questions.iter().map(|item| item.id.as_str()).collect();
        assert_eq!(ids.len(), questions.len(), "{id}: item IDs are unique");
        assert!(ids.iter().all(|id| !id.trim().is_empty()), "{id}");
        assert_eq!(
            definition.get("plan"),
            Some(&serde_json::Value::Null),
            "{id}: a legacy question never grants Plan authorization"
        );
        for column in 7..=10 {
            assert_eq!(
                row.get::<_, Option<String>>(column)
                    .expect("nullable authorization metadata"),
                None,
                "{id}: no authorization may be inferred from answer labels or missing input"
            );
        }
        count += 1;
    }
    assert_eq!(count, 6, "every recognized legacy question was inspected");
    let permissions: i64 = connection
        .query_row(
            "SELECT count(*) FROM question_interaction q JOIN human_request h
             ON h.id=q.request_id WHERE h.kind='permission'",
            [],
            |row| row.get(0),
        )
        .expect("permission companion count");
    assert_eq!(
        permissions, 0,
        "permission requests retain their original protocol"
    );
    let receipts: i64 = connection
        .query_row("SELECT count(*) FROM question_action_receipt", [], |row| {
            row.get(0)
        })
        .expect("command receipt count");
    assert_eq!(
        receipts, 0,
        "migration never invents a client command receipt"
    );
}

fn assert_question_migration(connection: &Connection, format: u32) {
    if format < 13 {
        assert_question_backfill(connection);
    } else {
        assert_literal_values(
            connection,
            &[
                (
                    "SELECT response FROM human_request WHERE id='req_format13'",
                    r#"{"draftAnswers":{"q1":["keep this draft"]}}"#,
                ),
                (
                    "SELECT purpose FROM question_interaction WHERE request_id='req_format13'",
                    "clarification",
                ),
                (
                    "SELECT definition FROM question_interaction WHERE request_id='req_format13'",
                    r#"{"origin":{"sessionId":"ses_fixture_0001"},"questions":[{"id":"q1","question":"Any correction?","header":"Notes","options":[]}],"plan":null,"initialMode":"deferred","handoffCompleted":false}"#,
                ),
            ],
            "published format-13 question metadata",
        );
    }
}

fn assert_legacy_input_receipts(connection: &Connection) {
    let mismatches: i64 = connection.query_row(
        "SELECT count(*) FROM session_input i LEFT JOIN session_input_receipt r ON r.input_id=i.id
         WHERE r.input_id IS NULL OR
           r.state <> CASE i.state WHEN 'consumed' THEN 'recorded'
                    WHEN 'failed' THEN 'failed' WHEN 'cancelled' THEN 'cancelled' ELSE 'admitted' END
           OR r.delivery <> i.delivery OR r.time_updated <> i.time_updated
           OR r.error IS NOT i.error OR r.turn_id IS NOT NULL OR r.applied_at IS NOT NULL
           OR r.stop_reason IS NOT NULL
           OR r.completed_at IS NOT
             CASE WHEN i.state IN ('failed','cancelled') THEN i.time_updated ELSE NULL END",
        [], |row| row.get(0),
    ).expect("compare migration receipts to historical inbox facts");
    assert_eq!(
        mismatches, 0,
        "legacy consumption must never imply model application"
    );
    let (inputs, receipts, usage): (i64, i64, i64) = connection
        .query_row(
            "SELECT (SELECT count(*) FROM session_input),
                (SELECT count(*) FROM session_input_receipt),
                (SELECT count(*) FROM session_context_usage)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("runtime ledger counts");
    assert_eq!(
        inputs, 6,
        "the backfill check must exercise every historical state"
    );
    assert_eq!(receipts, inputs);
    assert_eq!(usage, 0, "migration cannot invent measured context usage");
}

// ---------------------------------------------------------------------------
// The fixtures are the old formats, not the current one with pieces removed.
// ---------------------------------------------------------------------------

fn assert_fixture_is_the_old_format(fixture: &Fixture) {
    let dir = temp_dir();
    let connection = load_fixture(&dir.path().join("zuno.db"), fixture);
    let inventory = structure(&connection);
    let context = format!("{} fixture (format {})", fixture.release, fixture.format);

    assert_eq!(inventory.format, Some(fixture.format), "{context}: marker");
    assert_eq!(
        inventory.tables.len(),
        fixture.table_count,
        "{context}: table count; tables = {:?}",
        inventory.tables.keys().collect::<Vec<_>>()
    );
    let expected_other = if fixture.format >= 11 {
        [
            "experience_search_fts_insert",
            "experience_search_fts_update",
            "experience_search_fts_delete",
            "experience_search_cjk_fts_insert",
            "experience_search_cjk_fts_update",
            "experience_search_cjk_fts_delete",
        ]
        .into_iter()
        .map(|name| ("trigger".to_owned(), name.to_owned()))
        .collect()
    } else {
        BTreeSet::new()
    };
    assert_eq!(
        inventory.other_objects, expected_other,
        "{context}: unexpected non-table objects {:?}",
        inventory.other_objects
    );
    for step in steps_after(fixture.format) {
        for table in step.tables {
            assert!(
                !inventory.tables.contains_key(*table),
                "{context}: `{table}` belongs to the later step `{}`",
                step.name
            );
        }
        for index in step.indexes {
            assert!(
                !inventory.indexes.contains_key(*index),
                "{context}: index `{index}` belongs to the later step `{}`",
                step.name
            );
        }
        for (table, column) in step.columns {
            assert!(
                !inventory
                    .tables
                    .get(*table)
                    .is_some_and(|shape| shape.columns.contains_key(*column)),
                "{context}: `{table}.{column}` belongs to the later step `{}`",
                step.name
            );
        }
    }
    assert_literal_values(&connection, REPRESENTATIVE_VALUES, &context);
    if fixture.format >= 6 {
        assert_literal_values(&connection, LEARNING_REPRESENTATIVE_VALUES, &context);
    }
    if fixture.format >= 8 {
        assert_literal_values(&connection, VERIFICATION_REPRESENTATIVE_VALUES, &context);
    }
    if fixture.format >= 9 {
        assert_literal_values(&connection, MEMORY_POLICY_REPRESENTATIVE_VALUES, &context);
    }
    // Sanity: the old file is not already the current shape in disguise.
    assert_ne!(
        inventory,
        structure(&fresh_current()),
        "{context}: the fixture must differ structurally from a fresh database"
    );
}

#[test]
fn format_five_fixture_is_the_v0_0_3_database() {
    assert_fixture_is_the_old_format(&FORMAT_FIVE);
}

#[test]
fn format_six_fixture_is_the_v0_2_2_database() {
    assert_fixture_is_the_old_format(&FORMAT_SIX);
}

#[test]
fn format_seven_fixture_is_the_v0_6_7_database() {
    assert_fixture_is_the_old_format(&FORMAT_SEVEN);
}

#[test]
fn format_eight_fixture_is_the_v0_10_5_database() {
    assert_fixture_is_the_old_format(&FORMAT_EIGHT);
}

#[test]
fn format_nine_fixture_is_the_v0_10_21_database() {
    assert_fixture_is_the_old_format(&FORMAT_NINE);
}

// ---------------------------------------------------------------------------
// Upgrading a real old database yields the current structure and keeps every row.
// ---------------------------------------------------------------------------

fn assert_upgrade_preserves_rows_and_reaches_the_current_structure(fixture: &Fixture) {
    let dir = temp_dir();
    let path = dir.path().join("zuno.db");
    let mut connection = load_fixture(&path, fixture);
    let context = format!("upgrade of the {} fixture", fixture.release);
    let before = structure(&connection);
    let rows_before = snapshot_rows(&connection, &before);
    assert_eq!(before.format, Some(fixture.format));

    migration::apply(&mut connection).unwrap_or_else(|error| panic!("{context} failed: {error:#}"));

    // (c) The marker is the current one.
    let after = structure(&connection);
    assert_eq!(after.format, Some(migration::CURRENT_FORMAT), "{context}");
    assert_question_migration(&connection, fixture.format);
    assert_legacy_input_receipts(&connection);

    // (a) Every pre-existing value reads back byte-for-byte, and the literals the
    // fixture file spells out are still there. Only the marker row may differ.
    assert_rows_preserved(&connection, &rows_before, &["zuno_schema"]);
    assert_literal_values(&connection, REPRESENTATIVE_VALUES, &context);
    if fixture.format >= 6 {
        assert_literal_values(&connection, LEARNING_REPRESENTATIVE_VALUES, &context);
    }
    if fixture.format >= 8 {
        assert_literal_values(&connection, VERIFICATION_REPRESENTATIVE_VALUES, &context);
    }
    if fixture.format >= 9 {
        assert_literal_values(&connection, MEMORY_POLICY_REPRESENTATIVE_VALUES, &context);
    }

    // (d) Exactly the objects the remaining steps add are new; nothing was lost.
    let mut expected_tables: BTreeSet<&str> = before.tables.keys().map(String::as_str).collect();
    let mut expected_indexes: BTreeSet<&str> = before.indexes.keys().map(String::as_str).collect();
    for step in steps_after(fixture.format) {
        for table in step.tables {
            assert!(
                !before.tables.contains_key(*table),
                "{context}: `{table}` existed before `{}` ran",
                step.name
            );
            assert!(
                after.tables.contains_key(*table),
                "{context}: `{}` did not add `{table}`",
                step.name
            );
            expected_tables.insert(table);
        }
        for index in step.indexes {
            assert!(
                !before.indexes.contains_key(*index),
                "{context}: index `{index}` existed before `{}` ran",
                step.name
            );
            assert!(
                after.indexes.contains_key(*index),
                "{context}: `{}` did not add index `{index}`",
                step.name
            );
            expected_indexes.insert(index);
        }
        for (table, column) in step.columns {
            assert!(
                !before
                    .tables
                    .get(*table)
                    .is_some_and(|shape| shape.columns.contains_key(*column)),
                "{context}: `{table}.{column}` existed before `{}` ran",
                step.name
            );
            assert!(
                after.tables[*table].columns.contains_key(*column),
                "{context}: `{}` did not add `{table}.{column}`",
                step.name
            );
        }
    }
    let after_tables: BTreeSet<&str> = after.tables.keys().map(String::as_str).collect();
    assert_eq!(after_tables, expected_tables, "{context}: table set");
    // Automatic constraint indexes on new tables are legitimate additions too, so
    // the named-index set is a subset check rather than equality.
    let after_indexes: BTreeSet<&str> = after.indexes.keys().map(String::as_str).collect();
    assert!(
        expected_indexes.is_subset(&after_indexes),
        "{context}: missing indexes {:?}",
        expected_indexes
            .difference(&after_indexes)
            .collect::<Vec<_>>()
    );
    assert_eq!(after.tables.len(), CURRENT_TABLE_COUNT, "{context}");

    // Columns added by `ALTER TABLE` hold their declared defaults on the old row.
    if fixture.format < 7 {
        let (parent, depth): (Option<String>, i64) = connection
            .query_row(
                "SELECT parent_plan_id, stack_depth FROM work_plan \
                 WHERE session_id = 'ses_fixture_0001'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read the backfilled Plan-stack columns");
        assert_eq!((parent, depth), (None, 0), "{context}: Plan-stack defaults");
    }

    // (b) Structurally identical to a database created from nothing.
    assert_same_structure(&after, &structure(&fresh_current()), &context);
    let upgraded_rows = snapshot_rows(&connection, &after);

    // Reopening validates the upgraded file as current without touching it.
    drop(connection);
    let mut reopened = open::open_at(&path).expect("reopen the upgraded database");
    migration::apply(&mut reopened).expect("the upgraded database validates as current");
    assert_eq!(
        structure(&reopened),
        after,
        "{context}: reopen changed the structure"
    );
    assert_rows_preserved(&reopened, &rows_before, &["zuno_schema"]);
    assert_rows_preserved(&reopened, &upgraded_rows, &[]);
}

#[test]
fn format_five_fixture_upgrades_to_the_current_structure_and_keeps_every_row() {
    assert_upgrade_preserves_rows_and_reaches_the_current_structure(&FORMAT_FIVE);
}

#[test]
fn format_six_fixture_upgrades_to_the_current_structure_and_keeps_every_row() {
    assert_upgrade_preserves_rows_and_reaches_the_current_structure(&FORMAT_SIX);
}

#[test]
fn format_seven_fixture_upgrades_to_the_current_structure_and_keeps_every_row() {
    assert_upgrade_preserves_rows_and_reaches_the_current_structure(&FORMAT_SEVEN);
}

#[test]
fn format_eight_fixture_upgrades_to_the_current_structure_and_keeps_every_row() {
    assert_upgrade_preserves_rows_and_reaches_the_current_structure(&FORMAT_EIGHT);
}

#[test]
fn format_nine_fixture_upgrades_to_the_current_structure_and_keeps_every_row() {
    assert_upgrade_preserves_rows_and_reaches_the_current_structure(&FORMAT_NINE);
}

/// The one structural divergence between an upgraded database and a fresh one, and
/// the reason the comparison above matches columns by name: SQLite appends
/// `ALTER TABLE ... ADD COLUMN` columns after the existing ones, while
/// `CORE_SCHEMA_SQL` declares `parent_plan_id` and `stack_depth` right after `id`.
/// Zuno never reads `work_plan` positionally, so both layouts are the same format.
#[test]
fn upgraded_work_plan_columns_are_appended_not_interleaved() {
    let dir = temp_dir();
    let mut connection = load_fixture(&dir.path().join("zuno.db"), &FORMAT_SIX);
    let old_order = column_order(&connection, "work_plan");
    migration::apply(&mut connection).expect("upgrade format six");

    let upgraded = column_order(&connection, "work_plan");
    let fresh = column_order(&fresh_current(), "work_plan");
    assert_eq!(
        upgraded,
        [
            old_order.as_slice(),
            &["parent_plan_id".to_owned(), "stack_depth".to_owned()]
        ]
        .concat(),
        "an upgrade appends the Plan-stack columns after the v0.2.2 columns"
    );
    assert_eq!(
        &fresh[..4],
        ["session_id", "id", "parent_plan_id", "stack_depth"],
        "a fresh database declares the Plan-stack columns right after `id`"
    );
    assert_ne!(upgraded, fresh, "the cid order differs by construction");
    assert_eq!(
        upgraded.iter().collect::<BTreeSet<_>>(),
        fresh.iter().collect::<BTreeSet<_>>(),
        "the column sets are identical"
    );
}

/// The comparator is only evidence if it can fail. Prove it sees a missing index,
/// a missing column, a changed nullability, and a changed marker, and that two
/// fresh databases agree.
#[test]
fn the_structural_comparison_is_not_vacuous() {
    let reference = structure(&fresh_current());
    let mutated = fresh_current();
    assert_eq!(structure(&mutated), reference, "two fresh databases agree");

    mutated
        .execute_batch("DROP INDEX work_plan_goal_idx")
        .expect("drop an index");
    assert_ne!(structure(&mutated), reference, "a missing index is visible");
    mutated
        .execute_batch("CREATE INDEX work_plan_goal_idx ON work_plan (goal_id)")
        .expect("recreate the index with different spelling");
    assert_eq!(
        structure(&mutated),
        reference,
        "the same index spelled differently is the same structure"
    );

    mutated
        .execute_batch("ALTER TABLE work_plan DROP COLUMN stack_depth")
        .expect("drop a column");
    assert_ne!(
        structure(&mutated),
        reference,
        "a missing column is visible"
    );
    mutated
        .execute_batch("ALTER TABLE work_plan ADD COLUMN stack_depth integer DEFAULT 0")
        .expect("re-add the column without NOT NULL");
    assert_ne!(
        structure(&mutated),
        reference,
        "a column that lost its NOT NULL is visible"
    );

    let marker_only = fresh_current();
    marker_only
        .execute_batch("UPDATE zuno_schema SET format = 7 WHERE singleton = 1")
        .expect("rewrite the marker");
    assert_ne!(
        structure(&marker_only),
        reference,
        "a marker-only edit is visible"
    );
}

// ---------------------------------------------------------------------------
// A failed upgrade is a no-op: nothing created, nothing rewritten, marker unchanged.
// ---------------------------------------------------------------------------

/// `trace_v2` takes a bare `fn` pointer, so the statement log is a static and the
/// tests that install the hook take this lock for their whole duration.
static TRACE_LOCK: Mutex<()> = Mutex::new(());
static TRACED_STATEMENTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn record_statement(event: TraceEvent<'_>) {
    if let TraceEvent::Stmt(_, sql) = event {
        TRACED_STATEMENTS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(sql.to_owned());
    }
}

fn assert_failed_upgrade_leaves_the_database_untouched(fixture: &Fixture) {
    let _serial = TRACE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let dir = temp_dir();
    let path = dir.path().join("zuno.db");
    let mut connection = load_fixture(&path, fixture);
    let context = format!("failed upgrade of the {} fixture", fixture.release);

    // The trap: an unrelated index already owns the name of the upgrade's final
    // `CREATE INDEX`. Every earlier statement of every remaining step succeeds
    // inside the same transaction; only the last one fails.
    connection
        .execute_batch(&format!(
            "CREATE INDEX `{TRAP_INDEX}` ON `session` (`time_created`)"
        ))
        .expect("plant the conflicting index");
    let before = structure(&connection);
    let rows_before = snapshot_rows(&connection, &before);
    assert_eq!(before.format, Some(fixture.format));

    TRACED_STATEMENTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
    connection.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(record_statement));
    let error = migration::apply(&mut connection).expect_err("the trapped upgrade must fail");
    connection.trace_v2(TraceEventCodes::empty(), None);
    let traced = TRACED_STATEMENTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();

    // The failure came from the intended statement, reported as a schema failure.
    assert!(
        matches!(error, DbError::Schema { .. }),
        "{context}: expected DbError::Schema, got {error:?}"
    );
    let source = std::error::Error::source(&error)
        .expect("schema errors carry SQLite's error as their source")
        .to_string();
    assert!(
        source.contains(TRAP_INDEX) && source.contains("already exists"),
        "{context}: the failure did not come from the trapped index: {source}"
    );

    // SQLite's own trace: the write transaction opened, every remaining step ran its
    // first statement in order, the statement right before the trap was the last
    // one to run, the marker update never ran, and the transaction ended in
    // ROLLBACK rather than COMMIT.
    let position = |needle: &str| traced.iter().position(|sql| sql.contains(needle));
    let begin = position("BEGIN IMMEDIATE")
        .unwrap_or_else(|| panic!("{context}: no write transaction in {traced:#?}"));
    let mut previous = begin;
    for step in steps_after(fixture.format) {
        let at = position(step.first_statement).unwrap_or_else(|| {
            panic!(
                "{context}: step `{}` never ran `{}`; trace = {traced:#?}",
                step.name, step.first_statement
            )
        });
        assert!(
            at > previous,
            "{context}: step `{}` ran out of order; trace = {traced:#?}",
            step.name
        );
        previous = at;
    }
    let reached_trap = position(STATEMENT_BEFORE_TRAP).unwrap_or_else(|| {
        panic!("{context}: the upgrade never reached the trapped step: {traced:#?}")
    });
    assert!(
        reached_trap >= previous,
        "{context}: the trapped step ran before an earlier step; trace = {traced:#?}"
    );
    let rollback = traced
        .iter()
        .position(|sql| sql.trim_start().starts_with("ROLLBACK"))
        .unwrap_or_else(|| panic!("{context}: the failed upgrade did not roll back: {traced:#?}"));
    assert!(
        traced[reached_trap + 1..rollback]
            .iter()
            .all(|sql| sql.trim_start().starts_with("--")),
        "{context}: a top-level statement ran after the trap before rollback; \
         only SQLite's internal FTS flush statements may occur: {traced:#?}",
    );
    assert!(
        !traced.iter().any(|sql| sql.contains("UPDATE zuno_schema")),
        "{context}: the marker update ran before the upgrade had finished: {traced:#?}"
    );
    assert!(
        !traced
            .iter()
            .any(|sql| sql.trim_start().starts_with("COMMIT")),
        "{context}: something committed during a failed upgrade: {traced:#?}"
    );

    // The database is exactly what the fixture loaded: old marker, same rows, and
    // not one table, column, or index from any step left behind.
    let after = structure(&connection);
    assert_eq!(
        after.format,
        Some(fixture.format),
        "{context}: marker advanced"
    );
    assert_same_structure(&after, &before, &context);
    for step in steps_after(fixture.format) {
        for table in step.tables {
            assert!(
                !after.tables.contains_key(*table),
                "{context}: `{table}` from `{}` survived the rollback",
                step.name
            );
        }
        for (table, column) in step.columns {
            assert!(
                !after
                    .tables
                    .get(*table)
                    .is_some_and(|shape| shape.columns.contains_key(*column)),
                "{context}: `{table}.{column}` from `{}` survived the rollback",
                step.name
            );
        }
    }
    assert_rows_preserved(&connection, &rows_before, &[]);
    assert_literal_values(&connection, REPRESENTATIVE_VALUES, &context);

    // Once the trap is gone the same file upgrades cleanly, which is only possible
    // if the failed attempt left nothing half-applied behind.
    connection
        .execute_batch(&format!("DROP INDEX `{TRAP_INDEX}`"))
        .expect("remove the conflicting index");
    migration::apply(&mut connection)
        .unwrap_or_else(|error| panic!("{context}: retry after removing the trap: {error:#}"));
    assert_same_structure(
        &structure(&connection),
        &structure(&fresh_current()),
        &format!("{context}, retried"),
    );
    assert_rows_preserved(&connection, &rows_before, &["zuno_schema"]);
}

#[test]
fn a_failed_format_five_upgrade_leaves_the_v0_0_3_database_untouched() {
    assert_failed_upgrade_leaves_the_database_untouched(&FORMAT_FIVE);
}

#[test]
fn a_failed_format_six_upgrade_leaves_the_v0_2_2_database_untouched() {
    assert_failed_upgrade_leaves_the_database_untouched(&FORMAT_SIX);
}

#[test]
fn a_failed_format_seven_upgrade_leaves_the_v0_6_7_database_untouched() {
    assert_failed_upgrade_leaves_the_database_untouched(&FORMAT_SEVEN);
}

#[test]
fn a_failed_format_eight_upgrade_leaves_the_v0_10_5_database_untouched() {
    assert_failed_upgrade_leaves_the_database_untouched(&FORMAT_EIGHT);
}

#[test]
fn a_failed_format_nine_upgrade_leaves_the_v0_10_21_database_untouched() {
    assert_failed_upgrade_leaves_the_database_untouched(&FORMAT_NINE);
}

// ---------------------------------------------------------------------------
// Format 14: marker ordering, rollback after backfill, races, and closed shapes.
// ---------------------------------------------------------------------------

fn apply_traced(connection: &mut Connection) -> (Result<(), DbError>, Vec<String>) {
    let _serial = TRACE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    TRACED_STATEMENTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
    connection.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(record_statement));
    let outcome = migration::apply(connection);
    connection.trace_v2(TraceEventCodes::empty(), None);
    let traced = TRACED_STATEMENTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    (outcome, traced)
}

fn last_top_level_statement(traced: &[String]) -> Option<&str> {
    traced
        .iter()
        .rev()
        .map(|sql| sql.trim())
        .find(|sql| !sql.starts_with("--"))
}

/// Exact DDL as well as parsed shapes: a rejected database cannot be repaired,
/// have a CHECK rewritten, or lose a trigger as a side effect of validation.
fn schema_rows(connection: &Connection) -> Vec<Vec<Value>> {
    read_rows(
        connection,
        "sqlite_schema",
        &["type", "name", "tbl_name", "rootpage", "sql"].map(str::to_owned),
    )
}

fn object_ddl(connection: &Connection, names: &[&str]) -> String {
    names
        .iter()
        .map(|name| {
            let sql: String = connection
                .query_row(
                    "SELECT sql FROM sqlite_schema WHERE name=?1",
                    [name],
                    |row| row.get(0),
                )
                .expect("current object DDL");
            format!("{sql};\n")
        })
        .collect()
}

fn assert_rejected_without_mutation(connection: &mut Connection) -> DbError {
    let before = structure(connection);
    let rows_before = snapshot_rows(connection, &before);
    let ddl_before = schema_rows(connection);
    let error = migration::apply(connection).expect_err("invalid database must fail closed");
    assert_eq!(
        structure(connection),
        before,
        "rejection changed the schema or marker"
    );
    assert_eq!(
        schema_rows(connection),
        ddl_before,
        "rejection rewrote stored DDL"
    );
    assert_rows_preserved(connection, &rows_before, &[]);
    error
}

// Inspect future objects through SQLite's schema/pragma tables. Direct references
// to question_interaction inside this trigger would invalidate the trigger while
// format 14 rebuilds that table, aborting ALTER TABLE before the marker is reached.
const MARKER_SCHEMA_GUARD: &str = "
    SELECT CASE WHEN NEW.format <> 14
      THEN RAISE(ABORT,'marker attempted an intermediate format') END;
    SELECT CASE WHEN
      (SELECT count(*) FROM sqlite_schema WHERE name IN (
        'question_interaction','question_action_receipt',
        'question_interaction_purpose_authorization_idx',
        'session_input_receipt','session_input_receipt_turn_state_idx',
        'session_context_usage','session_context_usage_updated_idx')) <> 7
      THEN RAISE(ABORT,'marker attempted before complete runtime schema') END;
    SELECT CASE WHEN NOT EXISTS(
      SELECT 1 FROM pragma_table_info('session_execution_state') WHERE name='scheduling')
      THEN RAISE(ABORT,'marker attempted before scheduling') END;
    SELECT CASE WHEN NOT EXISTS(
      SELECT 1 FROM sqlite_schema WHERE name='question_interaction'
        AND instr(sql,'''goal_resume''') > 0)
      THEN RAISE(ABORT,'marker attempted before goal-resume purpose') END;
";

fn assert_receipt_backfill_precedes_the_only_marker(traced: &[String]) -> usize {
    let writes: Vec<_> = traced
        .iter()
        .enumerate()
        .filter(|(_, sql)| sql.trim_start().starts_with("UPDATE zuno_schema"))
        .collect();
    assert_eq!(writes.len(), 1, "one final marker write: {traced:#?}");
    let marker = writes[0].0;
    let backfill = traced
        .iter()
        .position(|sql| sql.contains("INSERT INTO session_input_receipt"))
        .expect("SQLite executed the input-receipt backfill");
    let begin = traced
        .iter()
        .position(|sql| sql.trim() == "BEGIN IMMEDIATE")
        .expect("the backfill belongs to the migration transaction");
    assert!(
        begin < backfill && backfill < marker,
        "backfill must precede the marker inside the transaction: {traced:#?}"
    );
    marker
}

#[test]
fn every_supported_upgrade_finishes_runtime_schema_and_backfills_before_the_marker_write() {
    for fixture in SUPPORTED_FIXTURES {
        let dir = temp_dir();
        let mut connection = load_fixture(&dir.path().join("zuno.db"), fixture);
        connection
            .execute_batch(&format!(
                "CREATE TRIGGER require_runtime_schema BEFORE UPDATE OF format ON zuno_schema
                 BEGIN {MARKER_SCHEMA_GUARD} END;"
            ))
            .expect("install the marker-order guard");
        let before = snapshot_rows(&connection, &structure(&connection));
        let (outcome, traced) = apply_traced(&mut connection);
        outcome.unwrap_or_else(|error| panic!("format {} marker order: {error:#}", fixture.format));
        assert_question_migration(&connection, fixture.format);
        assert_legacy_input_receipts(&connection);
        assert_rows_preserved(&connection, &before, &["zuno_schema"]);
        let marker = assert_receipt_backfill_precedes_the_only_marker(&traced);
        assert!(
            traced[marker + 1..]
                .iter()
                .all(|sql| { sql.trim_start().starts_with("--") || sql.trim() == "COMMIT" }),
            "only trigger checks and COMMIT may follow the marker: {traced:#?}"
        );
        assert_eq!(last_top_level_statement(&traced), Some("COMMIT"));
        assert_eq!(structure(&connection).format, Some(14));
    }
}

#[test]
fn a_marker_failure_rolls_back_runtime_receipts_question_rebuild_and_every_earlier_step() {
    for fixture in SUPPORTED_FIXTURES {
        let dir = temp_dir();
        let mut connection = load_fixture(&dir.path().join("zuno.db"), fixture);
        connection
            .execute_batch(&format!(
                "CREATE TRIGGER reject_runtime_marker BEFORE UPDATE OF format ON zuno_schema
                 BEGIN {MARKER_SCHEMA_GUARD}
                   SELECT RAISE(ABORT,'reject marker after complete runtime schema');
                 END;"
            ))
            .expect("install a failure after the real backfill");
        let before = structure(&connection);
        let rows_before = snapshot_rows(&connection, &before);
        let ddl_before = schema_rows(&connection);
        let (outcome, traced) = apply_traced(&mut connection);
        let error = outcome.expect_err("the marker trap must abort the whole migration");
        assert!(matches!(error, DbError::Schema { .. }), "{error:?}");
        assert!(
            std::error::Error::source(&error)
                .expect("SQLite cause")
                .to_string()
                .contains("reject marker after complete runtime schema"),
            "format {} did not reach the intended failure: {error:#}",
            fixture.format
        );
        assert_receipt_backfill_precedes_the_only_marker(&traced);
        assert_eq!(last_top_level_statement(&traced), Some("ROLLBACK"));
        assert!(!traced.iter().any(|sql| sql.trim() == "COMMIT"));
        assert_eq!(structure(&connection), before);
        assert_eq!(schema_rows(&connection), ddl_before);
        assert_rows_preserved(&connection, &rows_before, &[]);
        connection
            .execute_batch("DROP TRIGGER reject_runtime_marker")
            .expect("remove the deliberate marker failure");
        migration::apply(&mut connection).expect("retry the same database");
        assert_question_migration(&connection, fixture.format);
        assert_legacy_input_receipts(&connection);
        assert_rows_preserved(&connection, &rows_before, &["zuno_schema"]);
    }
}

#[test]
fn concurrent_fresh_and_every_supported_published_format_open_one_complete_schema() {
    for fixture in std::iter::once(None).chain(SUPPORTED_FIXTURES.iter().copied().map(Some)) {
        let dir = temp_dir();
        let path = dir.path().join("zuno.db");
        let initial = fixture.map_or_else(
            || open::open_at(&path).expect("open empty database"),
            |fixture| load_fixture(&path, fixture),
        );
        let before = snapshot_rows(&initial, &structure(&initial));
        drop(initial);
        // Establish WAL and the connection pragmas before racing the real
        // schema entry point. All four openers share only the database file.
        let connections: Vec<_> = (0..4)
            .map(|_| open::open_at(&path).expect("open concurrent connection"))
            .collect();
        let start = Arc::new(Barrier::new(connections.len()));
        let workers: Vec<_> = connections
            .into_iter()
            .map(|mut connection| {
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    let outcome = migration::apply(&mut connection);
                    (outcome, connection)
                })
            })
            .collect();
        let expected = structure(&fresh_current());
        for worker in workers {
            let (outcome, mut connection) = worker.join().expect("concurrent opener panicked");
            outcome.expect("every opener accepts the single committed upgrade");
            assert_same_structure(&structure(&connection), &expected, "concurrent open");
            assert_rows_preserved(&connection, &before, &["zuno_schema"]);
            if let Some(fixture) = fixture {
                assert_question_migration(&connection, fixture.format);
                assert_legacy_input_receipts(&connection);
            }
            let rows = snapshot_rows(&connection, &expected);
            migration::apply(&mut connection).expect("validate again without another backfill");
            assert_rows_preserved(&connection, &rows, &[]);
        }
    }
}

#[test]
fn unsupported_unmarked_and_marker_only_published_databases_are_unchanged() {
    for fixture in SUPPORTED_FIXTURES {
        for replacement in [None, Some(4), Some(13), Some(14), Some(15), Some(u32::MAX)] {
            if replacement == Some(fixture.format) {
                continue;
            }
            let dir = temp_dir();
            let mut connection = load_fixture(&dir.path().join("zuno.db"), fixture);
            if let Some(format) = replacement {
                connection
                    .execute("UPDATE zuno_schema SET format=?1", [format])
                    .expect("set the deliberately wrong marker");
            } else {
                connection
                    .execute_batch("DROP TABLE zuno_schema")
                    .expect("remove the marker");
            }
            let error = assert_rejected_without_mutation(&mut connection);
            if matches!(replacement, Some(13 | 14)) {
                assert!(matches!(error, DbError::Schema { .. }), "{error:?}");
            } else {
                assert!(
                    matches!(error, DbError::SchemaMismatch {
                expected: 14, observed
            } if observed == replacement),
                    "{error:?}"
                );
            }
        }
        let dir = temp_dir();
        let mut connection = load_fixture(&dir.path().join("zuno.db"), fixture);
        connection
            .execute("DELETE FROM zuno_schema", [])
            .expect("remove the marker row");
        assert!(matches!(
            assert_rejected_without_mutation(&mut connection),
            DbError::SchemaMismatch {
                expected: 14,
                observed: None
            }
        ));
    }
}

#[test]
fn corrupt_published_thirteen_is_rejected_before_any_format_fourteen_object_is_created() {
    for corruption in [
        "DROP TABLE question_interaction",
        "DROP TABLE question_action_receipt",
        "ALTER TABLE question_interaction DROP COLUMN authorization_input_id",
        "ALTER TABLE question_action_receipt DROP COLUMN time_created",
        "DROP INDEX question_interaction_purpose_authorization_idx",
        "DROP INDEX question_interaction_purpose_authorization_idx;
         CREATE INDEX question_interaction_purpose_authorization_idx
           ON question_interaction(authorization,purpose,request_id)",
        "ALTER TABLE session_execution_state DROP COLUMN scheduling",
        "ALTER TABLE session_execution_state DROP COLUMN scheduling;
         ALTER TABLE session_execution_state ADD COLUMN scheduling text",
    ] {
        let dir = temp_dir();
        let mut connection = load_fixture(&dir.path().join("zuno.db"), &FORMAT_THIRTEEN);
        connection
            .execute_batch(corruption)
            .expect("damage the published format-13 shape");
        let error = assert_rejected_without_mutation(&mut connection);
        assert!(
            matches!(error, DbError::Schema { .. }),
            "{corruption}: {error:?}"
        );
        let after = structure(&connection);
        assert_eq!(after.format, Some(13));
        for table in RUNTIME_CONSISTENCY.tables {
            assert!(!after.tables.contains_key(*table), "{corruption}: {table}");
        }
    }
}

#[test]
fn corrupt_format_twelve_sources_are_rejected_before_question_creation() {
    for corruption in [
        "DROP TABLE human_request",
        "ALTER TABLE human_request DROP COLUMN response",
        "DROP INDEX human_request_session_state_created_idx",
        "DROP INDEX human_request_goal_state_created_idx;
         CREATE INDEX human_request_goal_state_created_idx ON human_request(id)",
        "DROP TABLE resident_memory_provenance",
        "DROP TABLE memory_maintenance_state",
        "ALTER TABLE memory_candidate DROP COLUMN base_revision",
        "DROP INDEX resident_memory_provenance_candidate_idx",
        "DROP INDEX memory_candidate_path_status_updated_idx;
         CREATE INDEX memory_candidate_path_status_updated_idx ON memory_candidate(id)",
    ] {
        let dir = temp_dir();
        let mut connection = load_fixture(&dir.path().join("zuno.db"), &FORMAT_TWELVE);
        connection
            .execute_batch(corruption)
            .expect("construct the damaged released database");
        let error = assert_rejected_without_mutation(&mut connection);
        assert!(
            matches!(error, DbError::Schema { .. }),
            "{corruption}: {error:?}"
        );
        assert!(
            !structure(&connection)
                .tables
                .contains_key("question_interaction")
        );
    }
}

#[test]
fn every_supported_format_rejects_missing_human_requests_without_leaving_upgrade_objects() {
    for fixture in SUPPORTED_FIXTURES {
        let dir = temp_dir();
        let mut connection = load_fixture(&dir.path().join("zuno.db"), fixture);
        connection
            .execute_batch("DROP TABLE human_request")
            .expect("remove the required historical request table");
        let error = assert_rejected_without_mutation(&mut connection);
        assert!(
            matches!(error, DbError::Schema { .. }),
            "format {}: {error:?}",
            fixture.format
        );
    }
}

#[test]
fn current_question_tables_columns_and_required_index_must_exist() {
    for corruption in [
        "DROP TABLE question_interaction",
        "DROP TABLE question_action_receipt",
        "ALTER TABLE question_interaction DROP COLUMN risk_reason",
        "ALTER TABLE question_interaction DROP COLUMN authorization_input_id",
        "ALTER TABLE question_action_receipt DROP COLUMN time_created",
        "DROP INDEX question_interaction_purpose_authorization_idx",
    ] {
        let mut connection = fresh_current();
        connection
            .execute_batch(corruption)
            .expect("construct missing current objects");
        let error = assert_rejected_without_mutation(&mut connection);
        assert!(
            matches!(error, DbError::Schema { .. }),
            "{corruption}: {error:?}"
        );
    }
}

#[test]
fn current_question_shapes_reject_weakened_keys_checks_types_and_indexes() {
    // Use the actual current DDL as the unmodified control. The frozen format-13
    // questions.sql intentionally excludes goal_resume and is not a valid current
    // control; using it would mask every corruption behind that unrelated mismatch.
    let reference = fresh_current();
    let ddl = object_ddl(
        &reference,
        &[
            "question_interaction",
            "question_action_receipt",
            "question_interaction_purpose_authorization_idx",
        ],
    );
    for (original, changed) in [
        ("request_id text PRIMARY KEY", "request_id text"),
        ("purpose text NOT NULL", "purpose text"),
        ("mode text NOT NULL", "mode integer NOT NULL"),
        ("definition text NOT NULL", "definition text"),
        ("risk_reason text", "risk_reason integer"),
        ("risk_reason text", "\"risk_ reason\" text"),
        (
            "authorization_input_id text",
            "authorization_input_id integer",
        ),
        (
            "REFERENCES human_request(id) ON DELETE CASCADE",
            "REFERENCES human_request(id)",
        ),
        ("REFERENCES human_request(id)", "REFERENCES session(id)"),
        ("'goal_resume'))", "'goal_resume','other'))"),
        (",'goal_resume'", ""),
        ("'blocking','deferred'", "'blocking','deferred','silent'"),
        ("'approve','decline'", "'approve','decline','maybe'"),
        (
            "'applied','invalidated'",
            "'applied','invalidated','unknown'",
        ),
        ("'clarification'", "'CLARIFICATION'"),
        ("'required_input'", "'required_input '"),
        ("'clarification'", "'ifnotexistsclarification'"),
        ("CHECK (decision IN ('approve','decline'))", "CHECK (1)"),
        (
            "json_valid(definition) AND json_type(definition) = 'object'",
            "json_valid(definition)",
        ),
        (
            "json_valid(definition) AND json_type(definition) = 'object'",
            "json_valid(definition) AND json_type(definition) IN ('object','array')",
        ),
        ("command_id text NOT NULL", "command_id text"),
        ("command_json text NOT NULL", "command_json blob NOT NULL"),
        ("CHECK (json_valid(command_json))", "CHECK (1)"),
        ("CHECK (json_valid(receipt))", "CHECK (1)"),
        (
            "time_created integer NOT NULL",
            "time_created text NOT NULL",
        ),
        (
            "PRIMARY KEY (request_id, command_id)",
            "PRIMARY KEY (request_id)",
        ),
        (
            "CREATE INDEX question_interaction_purpose_authorization_idx",
            "CREATE UNIQUE INDEX question_interaction_purpose_authorization_idx",
        ),
        (
            "ON question_interaction(purpose, authorization, request_id)",
            "ON question_interaction(authorization, purpose, request_id)",
        ),
        (
            "ON question_interaction(purpose, authorization, request_id)",
            "ON question_interaction(purpose, authorization, request_id) WHERE decision='approve'",
        ),
        (
            "ON question_interaction(purpose, authorization, request_id)",
            "ON human_request(session_id, state, id)",
        ),
    ] {
        assert!(
            ddl.contains(original),
            "the corruption must actually change {original}"
        );
        let mut connection = fresh_current();
        connection
            .execute_batch("DROP TABLE question_interaction; DROP TABLE question_action_receipt;")
            .expect("replace only the question DDL with the test corruption");
        connection
            .execute_batch(&ddl.replace(original, changed))
            .unwrap_or_else(|error| panic!("construct {original} -> {changed}: {error}"));
        let error = assert_rejected_without_mutation(&mut connection);
        assert!(
            matches!(error, DbError::Schema { .. }),
            "{original} -> {changed}: {error:?}"
        );
    }
}

#[test]
fn current_runtime_tables_columns_and_required_indexes_must_exist() {
    for corruption in [
        "DROP TABLE session_input_receipt",
        "DROP TABLE session_context_usage",
        "ALTER TABLE session_input_receipt DROP COLUMN error",
        "ALTER TABLE session_context_usage DROP COLUMN state_json",
        "DROP INDEX session_input_receipt_turn_state_idx",
        "DROP INDEX session_context_usage_updated_idx",
    ] {
        let dir = temp_dir();
        let mut connection = load_fixture(&dir.path().join("zuno.db"), &FORMAT_THIRTEEN);
        migration::apply(&mut connection).expect("prepare a populated format 14");
        connection
            .execute_batch(corruption)
            .expect("remove a required runtime object");
        let error = assert_rejected_without_mutation(&mut connection);
        assert!(
            matches!(error, DbError::Schema { .. }),
            "{corruption}: {error:?}"
        );
        assert_eq!(structure(&connection).format, Some(14));
    }
}

#[test]
fn current_runtime_shapes_reject_weakened_keys_checks_types_and_indexes() {
    let reference = fresh_current();
    let ddl = object_ddl(
        &reference,
        &[
            "session_input_receipt",
            "session_input_receipt_turn_state_idx",
            "session_context_usage",
            "session_context_usage_updated_idx",
        ],
    );
    for (original, changed) in [
        (
            "input_id text NOT NULL PRIMARY KEY",
            "input_id text PRIMARY KEY",
        ),
        ("state text NOT NULL", "state text"),
        ("delivery text NOT NULL", "delivery blob NOT NULL"),
        ("'admitted','recorded'", "'admitted','RECORDED'"),
        (
            "CHECK (state <> 'applied' OR (turn_id IS NOT NULL AND applied_at IS NOT NULL))",
            "CHECK (1)",
        ),
        (
            "REFERENCES session_input(id) ON DELETE CASCADE",
            "REFERENCES session_input(id)",
        ),
        (
            "CREATE INDEX session_input_receipt_turn_state_idx",
            "CREATE UNIQUE INDEX session_input_receipt_turn_state_idx",
        ),
        ("(turn_id,state,input_id)", "(state,turn_id,input_id)"),
        (
            "(turn_id,state,input_id)",
            "(turn_id,state,input_id) WHERE state='applied'",
        ),
        ("`source` text NOT NULL", "`source` text"),
        ("`revision` integer NOT NULL", "`revision` text NOT NULL"),
        ("CHECK (`context_epoch` >= 0)", "CHECK (1)"),
        ("CHECK (json_valid(`state_json`))", "CHECK (1)"),
        (
            "PRIMARY KEY (`session_id`, `source`)",
            "PRIMARY KEY (`session_id`)",
        ),
        (
            "REFERENCES `session`(`id`) ON DELETE CASCADE",
            "REFERENCES `session`(`id`)",
        ),
        (
            "(`time_updated`, `session_id`, `source`)",
            "(`session_id`, `time_updated`, `source`)",
        ),
    ] {
        assert!(ddl.contains(original), "corruption must change {original}");
        let dir = temp_dir();
        let mut connection = load_fixture(&dir.path().join("zuno.db"), &FORMAT_THIRTEEN);
        migration::apply(&mut connection).expect("prepare populated current schema");
        connection
            .execute_batch("DROP TABLE session_input_receipt; DROP TABLE session_context_usage;")
            .expect("replace only runtime DDL");
        connection
            .execute_batch(&ddl.replace(original, changed))
            .unwrap_or_else(|error| panic!("construct {original} -> {changed}: {error}"));
        let error = assert_rejected_without_mutation(&mut connection);
        assert!(
            matches!(error, DbError::Schema { .. }),
            "{original} -> {changed}: {error:?}"
        );
    }
}

#[test]
fn current_input_receipts_reject_significant_whitespace_in_quoted_column_names() {
    let mut connection = fresh_current();
    connection
        .execute_batch("ALTER TABLE session_input_receipt RENAME COLUMN input_id TO \"input_ id\";")
        .expect("construct a different parsed column and its updated index");
    assert!(
        connection
            .prepare("SELECT input_id FROM session_input_receipt")
            .is_err(),
        "the column rename must break the actual input-receipt query"
    );
    assert!(matches!(
        assert_rejected_without_mutation(&mut connection),
        DbError::Schema { .. }
    ));
}

#[test]
fn current_context_usage_rejects_significant_whitespace_in_quoted_column_names() {
    let mut connection = fresh_current();
    connection
        .execute_batch("ALTER TABLE session_context_usage RENAME COLUMN source TO \"so urce\";")
        .expect("construct a different parsed column and its updated key/index");
    assert!(
        connection
            .prepare("SELECT source FROM session_context_usage")
            .is_err(),
        "the column rename must break the actual context-usage query"
    );
    assert!(matches!(
        assert_rejected_without_mutation(&mut connection),
        DbError::Schema { .. }
    ));
}

#[path = "migration_fixtures/preview.rs"]
mod preview_lineage;

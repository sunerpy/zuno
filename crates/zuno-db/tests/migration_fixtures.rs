//! Upgrades from databases that released Zuno versions actually wrote.
//!
//! The in-crate migration tests build their "old" databases by taking the current
//! schema and removing pieces. That proves each upgrade step is additive against
//! today's `create_current`, but it cannot notice a column an old release never
//! had, or an object an old release wrote that the current schema no longer
//! describes. The fixtures under `tests/fixtures/` close that gap: each one is the
//! DDL a tagged release's `migration::create_current` executed, recovered from that
//! tag's `schema.rs` and `migration/mod.rs` (the header of every file names the
//! exact `git show` commands), followed by representative rows.
//!
//! | fixture        | format | release | what the upgrade must add                          |
//! |----------------|--------|---------|----------------------------------------------------|
//! | `format-5.sql` | 5      | v0.0.3  | learning flywheel, Plan stack, verification ledger |
//! | `format-6.sql` | 6      | v0.2.2  | Plan stack, verification ledger                    |
//! | `format-7.sql` | 7      | v0.6.7  | verification ledger                                |
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

use rusqlite::trace::{TraceEvent, TraceEventCodes};
use rusqlite::types::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Mutex, PoisonError};
use zuno_db::{Connection, migration, open};
use zuno_error::DbError;
use zuno_paths::DbLocation;

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

/// Every table `sqlite_master` lists once the current schema is in place.
const CURRENT_TABLE_COUNT: usize = 39;

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

/// The index created by the final DDL statement of every upgrade path. Index names
/// are database-global, so an unrelated index that already owns this name makes
/// exactly that statement fail after everything before it ran inside the same
/// transaction. SQLite rejects the duplicate while *preparing* the statement, so
/// it never reaches `SQLITE_TRACE_STMT`; the statement immediately before it is
/// therefore the last one the trace can show before the rollback.
const TRAP_INDEX: &str = "verification_receipt_session_time_idx";
const STATEMENT_BEFORE_TRAP: &str = "CREATE UNIQUE INDEX `verification_receipt_call_idx`";

fn steps_after(format: u32) -> &'static [&'static Step] {
    match format {
        5 => &[&LEARNING, &PLAN_STACK, &VERIFICATION],
        6 => &[&PLAN_STACK, &VERIFICATION],
        7 => &[&VERIFICATION],
        other => panic!("no fixture describes format {other}"),
    }
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
        Some(
            connection
                .query_row(
                    "SELECT format FROM zuno_schema WHERE singleton = 1",
                    [],
                    |row| row.get::<_, u32>(0),
                )
                .expect("read the format marker"),
        )
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
            "SELECT {projection} FROM `{table}` ORDER BY rowid"
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
    assert!(
        inventory.other_objects.is_empty(),
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
                !inventory.tables[*table].columns.contains_key(*column),
                "{context}: `{table}.{column}` belongs to the later step `{}`",
                step.name
            );
        }
    }
    assert_literal_values(&connection, REPRESENTATIVE_VALUES, &context);
    if fixture.format >= 6 {
        assert_literal_values(&connection, LEARNING_REPRESENTATIVE_VALUES, &context);
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

    // (a) Every pre-existing value reads back byte-for-byte, and the literals the
    // fixture file spells out are still there. Only the marker row may differ.
    assert_rows_preserved(&connection, &rows_before, &["zuno_schema"]);
    assert_literal_values(&connection, REPRESENTATIVE_VALUES, &context);
    if fixture.format >= 6 {
        assert_literal_values(&connection, LEARNING_REPRESENTATIVE_VALUES, &context);
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
                !before.tables[*table].columns.contains_key(*column),
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
        reached_trap > previous,
        "{context}: the trapped step ran before an earlier step; trace = {traced:#?}"
    );
    let rollback = traced
        .iter()
        .position(|sql| sql.trim_start().starts_with("ROLLBACK"))
        .unwrap_or_else(|| panic!("{context}: the failed upgrade did not roll back: {traced:#?}"));
    assert_eq!(
        rollback,
        reached_trap + 1,
        "{context}: something ran between the trapped statement and the rollback \
         (the trapped `CREATE INDEX` itself fails at prepare and is never traced); \
         trace = {traced:#?}"
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
                !after.tables[*table].columns.contains_key(*column),
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

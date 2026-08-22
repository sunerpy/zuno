//! `VACUUM` is explicit, guarded by free space, and never a side effect of a prune.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use zuno_db::prune::{
    PruneRequest, RemoteUnshare, SharedSession, UnshareError, execute as prune_execute,
    preview as prune_preview,
};
use zuno_db::retention::{
    DAY_MILLIS, Liveness, LivenessProbe, RetentionReport, RetentionRequest, RetentionScope, select,
};
use zuno_db::vacuum::{
    Availability, DEFAULT_LARGEST_SESSIONS, DatabaseSize, DiskSpace, INTEGRITY_OK, SystemDiskSpace,
    VacuumError, checkpoint, database_path, database_size, format_bytes, integrity_check, stats,
    to_json, vacuum,
};
use zuno_db::{Connection, migration, open};

const NOW: i64 = 400 * DAY_MILLIS;
const OLD: i64 = 10 * DAY_MILLIS;
const RECENT: i64 = 399 * DAY_MILLIS;
/// Sessions per age band. Enough rows that a half-prune frees whole pages
/// rather than a rounding error.
const SESSIONS_PER_BAND: usize = 24;
/// Parts per session; with [`PART_PAYLOAD_BYTES`] this puts roughly 1.5 MiB of
/// payload in the file, so a reclaim is measurable in pages and not in noise.
const PARTS_PER_SESSION: usize = 8;
const PART_PAYLOAD_BYTES: usize = 4096;

struct Reachable;

impl LivenessProbe for Reachable {
    fn probe(&self) -> Liveness {
        Liveness::Reachable {
            active_session_ids: BTreeSet::new(),
        }
    }
}

struct WorkingRemote;

impl RemoteUnshare for WorkingRemote {
    fn unshare(&self, _session: &SharedSession) -> Result<(), UnshareError> {
        Ok(())
    }
}

/// A disk that reports whatever the test decides, and counts its own calls so a
/// test can prove the guard was consulted rather than skipped.
struct FakeDisk {
    answer: Availability,
    calls: Cell<usize>,
}

impl FakeDisk {
    fn known(bytes: u64) -> Self {
        Self {
            answer: Availability::Known { bytes },
            calls: Cell::new(0),
        }
    }

    fn unknown(reason: &str) -> Self {
        Self {
            answer: Availability::Unknown {
                reason: reason.to_owned(),
            },
            calls: Cell::new(0),
        }
    }
}

impl DiskSpace for FakeDisk {
    fn available_bytes(&self, _path: &Path) -> Availability {
        self.calls.set(self.calls.get() + 1);
        self.answer.clone()
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    path: PathBuf,
    connection: Connection,
}

impl Fixture {
    /// A real file database, because a vacuum's entire observable effect is the
    /// length of a file. An in-memory database has none.
    fn build() -> Self {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("zuno.db");
        let mut connection = open::open_at(&path).expect("open file database");
        migration::apply(&mut connection).expect("apply schema");
        seed(&mut connection);
        checkpoint(&connection).expect("fold the seed into the main file");
        Self {
            _directory: directory,
            path,
            connection,
        }
    }

    /// The descendant-closed set todo 81 selects for the old band only.
    fn old_band(&self) -> RetentionReport {
        let report = select(
            &self.connection,
            &RetentionRequest::new(90, RetentionScope::AllProjects, NOW),
            &Reachable,
        )
        .expect("select retention candidates");
        assert_eq!(
            report.selected.len(),
            SESSIONS_PER_BAND,
            "the fixture's two age bands must be separable; selecting everything or \
             nothing makes every reclaim assertion below vacuous"
        );
        report
    }

    fn prune(&mut self, selection: &RetentionReport) {
        let outcome = prune_execute(
            &mut self.connection,
            selection,
            &PruneRequest::delete().confirmed(),
            &WorkingRemote,
        )
        .expect("prune the old band");
        assert_eq!(
            outcome.changed_sessions, SESSIONS_PER_BAND as u64,
            "the prune must actually remove the selected sessions"
        );
    }

    fn size(&self) -> DatabaseSize {
        database_size(&self.path)
    }

    fn freelist_pages(&self) -> u64 {
        stats(&self.connection, 0)
            .expect("read stats")
            .freelist_pages
    }
}

fn seed(connection: &mut Connection) {
    let transaction = connection.transaction().expect("begin seed transaction");
    transaction
        .execute(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
             VALUES ('prj_a', '/srv/a', 1, 1, '[]')",
            [],
        )
        .expect("insert project");

    let payload = "x".repeat(PART_PAYLOAD_BYTES);
    for (band, updated) in [("old", OLD), ("new", RECENT)] {
        for index in 0..SESSIONS_PER_BAND {
            let session_id = format!("ses_{band}_{index:03}");
            transaction
                .execute(
                    "INSERT INTO session
                       (id, project_id, slug, directory, title, version,
                        time_created, time_updated)
                     VALUES (?1, 'prj_a', ?2, '/srv/a', ?3, '1.18.13', 1, ?4)",
                    rusqlite::params![
                        session_id,
                        format!("slug-{band}-{index:03}"),
                        format!("{band} session {index}"),
                        updated
                    ],
                )
                .expect("insert session");
            for part in 0..PARTS_PER_SESSION {
                let message_id = format!("msg_{session_id}_{part:02}");
                transaction
                    .execute(
                        "INSERT INTO message (id, session_id, time_created, time_updated, data)
                         VALUES (?1, ?2, 1, 1, '{\"role\":\"user\"}')",
                        rusqlite::params![message_id, session_id],
                    )
                    .expect("insert message");
                transaction
                    .execute(
                        "INSERT INTO part
                           (id, message_id, session_id, time_created, time_updated, data)
                         VALUES (?1, ?2, ?3, 1, 1, ?4)",
                        rusqlite::params![
                            format!("prt_{session_id}_{part:02}"),
                            message_id,
                            session_id,
                            format!(r#"{{"type":"text","text":"needle {payload}"}}"#)
                        ],
                    )
                    .expect("insert part");
            }
        }
    }
    transaction.commit().expect("commit seed");
}

fn source_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")];
    while let Some(directory) = stack.pop() {
        let entries = std::fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()));
        for entry in entries {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

#[test]
fn vacuum_a_prune_alone_reclaims_nothing_and_an_explicit_vacuum_reclaims_bytes() {
    let mut fixture = Fixture::build();

    // Without this, none of the numbers below mean anything: with `auto_vacuum`
    // enabled SQLite would return freed pages to the filesystem on commit, the
    // prune would report a nonzero reclaim, and reclamation would be implicit.
    assert_eq!(
        open::query_int(
            &fixture.connection,
            "auto_vacuum",
            &zuno_paths::DbLocation::File(fixture.path.clone()),
        )
        .expect("read auto_vacuum"),
        0,
        "the Zuno schema must leave auto_vacuum NONE"
    );

    let baseline = fixture.size();
    assert!(
        baseline.main_bytes > 1_000_000,
        "the fixture must be large enough for a reclaim to exceed page granularity: {baseline:?}"
    );

    let selection = fixture.old_band();
    fixture.prune(&selection);
    checkpoint(&fixture.connection).expect("fold the delete into the main file");
    let after_prune = fixture.size();

    assert_eq!(
        baseline.reclaimed_since(after_prune),
        0,
        "the prune alone must return nothing to the filesystem \
         (baseline {baseline:?}, after prune {after_prune:?})"
    );
    assert!(
        after_prune.main_bytes >= baseline.main_bytes,
        "deleting rows must not shrink the file: {baseline:?} then {after_prune:?}"
    );
    let freed_pages = fixture.freelist_pages();
    assert!(
        freed_pages > 0,
        "the prune must have freed pages inside the file; that is the whole reason \
         it reclaimed nothing"
    );

    let report = vacuum(&mut fixture.connection, &SystemDiskSpace).expect("vacuum");

    assert_eq!(report.path, fixture.path);
    assert_eq!(report.before, after_prune, "vacuum must measure, not guess");
    assert_eq!(report.freelist_pages_before, freed_pages);
    assert_eq!(
        report.freelist_pages_after, 0,
        "a completed rewrite leaves no freelist"
    );
    assert!(
        report.reclaimed_bytes > 0,
        "the explicit vacuum must report the bytes it returned: {report:?}"
    );
    assert!(
        report.reclaimed_bytes > baseline.main_bytes / 4,
        "half the payload was deleted, so a reclaim near zero means the rewrite did \
         not happen: reclaimed {} of {}",
        report.reclaimed_bytes,
        baseline.main_bytes
    );
    assert_eq!(
        report.reclaimed_bytes,
        report.before.reclaimed_since(report.after),
        "the reported reclaim must be the difference between the two measurements"
    );
    assert_eq!(
        report.after,
        fixture.size(),
        "the file must really be smaller"
    );
    assert!(
        !report.fts_rebuild_required,
        "migration::apply installs no FTS objects"
    );

    // Printed so the evidence behind the two headline numbers can be regenerated
    // with `--nocapture` instead of re-derived by hand.
    println!(
        "seeded main={} after-prune main={} prune reclaimed={} freelist={} \
         vacuumed main={} vacuum reclaimed={}",
        baseline.main_bytes,
        after_prune.main_bytes,
        baseline.reclaimed_since(after_prune),
        freed_pages,
        report.after.main_bytes,
        report.reclaimed_bytes,
    );
}

#[test]
fn vacuum_refuses_when_free_disk_is_under_the_database_size() {
    let mut fixture = Fixture::build();
    let size = fixture.size();
    let short_by = 64 * 1024;
    let available = size.main_bytes - short_by;
    let disk = FakeDisk::known(available);

    let error = vacuum(&mut fixture.connection, &disk)
        .expect_err("a filesystem smaller than the database must refuse the rewrite");

    assert_eq!(
        disk.calls.get(),
        1,
        "the guard must be consulted exactly once"
    );
    match &error {
        VacuumError::InsufficientDiskSpace {
            path,
            required_bytes,
            available_bytes,
        } => {
            assert_eq!(path, &fixture.path);
            assert_eq!(*required_bytes, size.main_bytes);
            assert_eq!(*available_bytes, available);
        }
        other => panic!("expected an insufficient-space refusal, got {other:?}"),
    }

    let message = error.to_string();
    for expected in [
        fixture.path.display().to_string(),
        size.main_bytes.to_string(),
        format_bytes(size.main_bytes),
        available.to_string(),
        format_bytes(available),
        short_by.to_string(),
        format_bytes(short_by),
    ] {
        assert!(
            message.contains(&expected),
            "the refusal must name {expected}: {message}"
        );
    }
    assert!(
        message.contains("ZUNO_DB"),
        "the refusal must be actionable: {message}"
    );

    assert_eq!(
        fixture.size(),
        size,
        "a refused vacuum must not have touched the file"
    );
}

#[test]
fn vacuum_proceeds_when_free_space_is_exactly_the_database_size() {
    let mut fixture = Fixture::build();
    let selection = fixture.old_band();
    fixture.prune(&selection);
    let size = fixture.size();

    // The guard is `available < required`, so equality is the first passing
    // value. Pinned because an off-by-one here refuses every vacuum on a disk
    // that is exactly large enough.
    let disk = FakeDisk::known(size.main_bytes);
    let report = vacuum(&mut fixture.connection, &disk).expect("equality must pass the guard");
    assert_eq!(disk.calls.get(), 1);
    assert!(report.reclaimed_bytes > 0);
}

#[test]
fn vacuum_proceeds_when_free_space_cannot_be_established_and_says_why() {
    let mut fixture = Fixture::build();
    let selection = fixture.old_band();
    fixture.prune(&selection);

    let disk = FakeDisk::unknown("this platform cannot answer");
    let report = vacuum(&mut fixture.connection, &disk)
        .expect("an unanswerable guard must not block maintenance");

    assert_eq!(disk.calls.get(), 1);
    assert_eq!(
        report.available_bytes,
        Availability::Unknown {
            reason: "this platform cannot answer".to_owned()
        },
        "the report must record that the guard could not be evaluated"
    );
    assert!(report.reclaimed_bytes > 0);
}

#[test]
fn vacuum_integrity_check_passes_on_a_freshly_pruned_database() {
    let mut fixture = Fixture::build();
    let selection = fixture.old_band();
    fixture.prune(&selection);

    let pruned = integrity_check(&fixture.connection).expect("check a pruned database");
    assert_eq!(pruned.integrity, [INTEGRITY_OK]);
    assert_eq!(
        pruned.foreign_key_violations,
        [],
        "todo 82's explicit delete order and orphan sweep must leave no dangling rows"
    );
    assert!(pruned.is_ok());

    vacuum(&mut fixture.connection, &SystemDiskSpace).expect("vacuum");

    let vacuumed = integrity_check(&fixture.connection).expect("check a vacuumed database");
    assert!(vacuumed.is_ok(), "{vacuumed:?}");
}

#[test]
fn vacuum_integrity_check_reports_a_dangling_reference_rather_than_calling_it_ok() {
    // `PRAGMA integrity_check` alone answers "ok" for a structurally sound file
    // whose references do not resolve, which is exactly the damage a connection
    // that inherited `foreign_keys = OFF` leaves behind. Written with the pragma
    // turned off so the row can be inserted at all.
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("orphan.db");
    let mut connection = open::open_at(&path).expect("open");
    migration::apply(&mut connection).expect("apply schema");
    connection
        .pragma_update(None, "foreign_keys", "OFF")
        .expect("disable foreign keys for this connection only");
    connection
        .execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
             VALUES ('prt_orphan', 'msg_absent', 'ses_absent', 1, 1, '{}')",
            [],
        )
        .expect("insert an orphan part");

    let report = integrity_check(&connection).expect("check");
    assert_eq!(
        report.integrity,
        [INTEGRITY_OK],
        "the file itself is structurally fine, which is the point"
    );
    assert!(!report.is_ok(), "{report:?}");
    let violation = report
        .foreign_key_violations
        .first()
        .expect("the dangling reference must be reported");
    assert_eq!(violation.table, "part");
    assert_eq!(violation.parent, "message");
}

#[test]
fn vacuum_refuses_a_connection_that_has_no_file_to_rewrite() {
    let mut connection =
        open::open_shared_memory("zuno-vacuum-no-file").expect("open shared memory database");
    assert!(database_path(&connection).is_none());
    let error = vacuum(&mut connection, &SystemDiskSpace)
        .expect_err("an in-memory database cannot be compacted");
    assert!(matches!(error, VacuumError::NotAFileDatabase));
    assert!(
        error.to_string().contains("file database"),
        "{}",
        error.to_string()
    );
}

#[test]
fn vacuum_stats_counts_every_table_the_live_schema_actually_has() {
    let fixture = Fixture::build();
    let summary = stats(&fixture.connection, DEFAULT_LARGEST_SESSIONS).expect("stats");

    // Read from `sqlite_master`, not from a list in this test: the plan's
    // milestone text and todo 82 disagreed about how many tables exist, and the
    // schema is the only authority. `schema::TABLE_COUNT` is the application tables
    // `schema::up` creates; format initialization adds its one marker table.
    let names: Vec<&str> = summary
        .tables
        .iter()
        .map(|entry| entry.table.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "account",
            "account_state",
            "agent_job",
            "control_account",
            "credential",
            "data_migration",
            "event",
            "event_sequence",
            "message",
            "part",
            "permission",
            "project",
            "project_directory",
            "session",
            "session_context_epoch",
            "session_input",
            "session_message",
            "session_share",
            "todo",
            "workspace",
            "zuno_schema",
        ],
        "the reported inventory must be the schema's, in name order"
    );
    assert_eq!(summary.tables.len(), zuno_db::schema::TABLE_COUNT + 1);

    let expected_sessions = 2 * SESSIONS_PER_BAND;
    let expected_parts = expected_sessions * PARTS_PER_SESSION;
    assert_eq!(
        summary
            .table("session")
            .expect("session must be reported")
            .rows,
        expected_sessions as u64
    );
    assert_eq!(
        summary.table("part").expect("part must be reported").rows,
        expected_parts as u64
    );
    assert!(
        summary.total_rows >= (expected_sessions + 2 * expected_parts) as u64,
        "total_rows must be the sum of every table: {}",
        summary.total_rows
    );
}

#[test]
fn vacuum_stats_reports_page_geometry_and_a_wal_size_separate_from_the_main_file() {
    let mut fixture = Fixture::build();
    let before = stats(&fixture.connection, 0).expect("stats");
    assert_eq!(before.path.as_deref(), Some(fixture.path.as_path()));
    assert!(before.page_size >= 512, "{}", before.page_size);
    assert!(before.page_count > 0);
    assert_eq!(
        before.size.main_bytes,
        before.page_size * before.page_count,
        "the file length must be page_size * page_count after a truncating checkpoint"
    );
    assert_eq!(
        before.size.wal_bytes, 0,
        "the fixture checkpointed with TRUNCATE, so the WAL is empty"
    );

    // WAL mode is what makes a separate WAL figure worth reporting at all
    // (`database.ts:22-33`): an uncheckpointed write lands there and not in the
    // main file, so a summary that folded the two together would report the
    // database as unchanged.
    let selection = fixture.old_band();
    fixture.prune(&selection);
    let after = stats(&fixture.connection, 0).expect("stats");
    assert!(
        after.size.wal_bytes > 0,
        "an uncheckpointed delete must be visible as WAL bytes: {:?}",
        after.size
    );
    assert_eq!(
        after.size.main_bytes, before.size.main_bytes,
        "and must not have changed the main file"
    );
    assert!(after.size.total_bytes() > before.size.total_bytes());
}

#[test]
fn vacuum_stats_ranks_sessions_by_part_bytes_and_agrees_with_the_prune_preview() {
    let fixture = Fixture::build();
    let summary = stats(&fixture.connection, 3).expect("stats");
    assert_eq!(
        summary.largest_sessions.len(),
        3,
        "the limit must be honoured"
    );

    for session in &summary.largest_sessions {
        assert_eq!(session.part_rows, PARTS_PER_SESSION as u64);
        assert!(
            session.part_bytes > (PARTS_PER_SESSION * PART_PAYLOAD_BYTES) as u64,
            "{session:?}"
        );
        assert!(!session.title.is_empty(), "a report must be readable");
    }
    let bytes: Vec<u64> = summary
        .largest_sessions
        .iter()
        .map(|session| session.part_bytes)
        .collect();
    let mut descending = bytes.clone();
    descending.sort_unstable_by(|left, right| right.cmp(left));
    assert_eq!(bytes, descending, "largest first");

    // One definition of "bytes", shared with todo 82. Two definitions would make
    // the number `db stats` shows disagree with the number `session delete`
    // previews for the same rows.
    let heaviest = summary
        .largest_sessions
        .first()
        .expect("at least one session");
    let single = RetentionReport {
        age_cutoff_ms: NOW,
        liveness: Liveness::Unreachable,
        selected: vec![zuno_db::retention::RetentionCandidate {
            id: heaviest.session_id.clone(),
            reasons: Vec::new(),
        }],
        excluded: Vec::new(),
    };
    let preview = prune_preview(&fixture.connection, &single).expect("preview one session");
    let part = preview.table("part").expect("part impact");
    assert_eq!(part.rows, heaviest.part_rows);
    assert_eq!(part.bytes, heaviest.part_bytes);
}

#[test]
fn vacuum_stats_json_carries_every_reported_field() {
    let fixture = Fixture::build();
    let summary = stats(&fixture.connection, 2).expect("stats");
    let json = to_json(&summary).expect("encode stats");
    let object = json.as_object().expect("an object");

    for key in [
        "path",
        "size",
        "page_size",
        "page_count",
        "freelist_pages",
        "tables",
        "total_rows",
        "largest_sessions",
    ] {
        assert!(object.contains_key(key), "stats JSON must carry {key}");
    }
    let size = object["size"].as_object().expect("size object");
    for key in ["main_bytes", "wal_bytes", "shm_bytes"] {
        assert!(size.contains_key(key), "size JSON must carry {key}");
    }
    assert_eq!(
        object["largest_sessions"].as_array().expect("array").len(),
        2
    );
}

#[test]
fn vacuum_reports_that_the_opt_in_search_index_has_to_be_rebuilt() {
    let mut fixture = Fixture::build();
    zuno_db::fts::ensure(&mut fixture.connection).expect("install the opt-in FTS objects");
    let selection = fixture.old_band();
    fixture.prune(&selection);

    let report = vacuum(&mut fixture.connection, &SystemDiskSpace).expect("vacuum");
    assert!(
        report.fts_rebuild_required,
        "a rewrite renumbers the rowids the external-content index uses as document \
         ids, so the caller has to be told (fts.rs:240-244)"
    );

    // And the obligation is dischargeable: rebuilding restores search over the
    // surviving rows. A vacuum that had rebuilt the index itself would be doing
    // hidden work; a vacuum that never reported it would leave search wrong.
    zuno_db::fts::rebuild(&fixture.connection).expect("rebuild after the rewrite");
    let hits = zuno_db::fts::search(&fixture.connection, "needle", 5).expect("search");
    assert!(
        !hits.is_empty(),
        "surviving messages must still be findable"
    );
}

#[test]
fn vacuum_is_never_reachable_as_a_side_effect_of_another_module() {
    let files = source_files();
    assert!(
        files.len() >= 12,
        "the scan found only {} source files under crates/zuno-db/src; it is looking in \
         the wrong place and would pass vacuously",
        files.len()
    );

    let mut offenders = Vec::new();
    let mut inspected_lines = 0_usize;
    for file in &files {
        let owns_vacuum = file.file_name().is_some_and(|name| name == "vacuum.rs");
        let text = std::fs::read_to_string(file)
            .unwrap_or_else(|error| panic!("read {}: {error}", file.display()));
        for (index, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            // Doc comments legitimately discuss `VACUUM`: `fts.rs` documents the
            // rebuild obligation and `open.rs` explains why the WAL sidecar
            // suffixes are a constant. Only executable lines are in scope.
            if trimmed.starts_with("//") {
                continue;
            }
            inspected_lines += 1;
            let reason = if !owns_vacuum && line.contains("VACUUM") {
                Some("issues a VACUUM statement")
            } else if !owns_vacuum && line.contains("vacuum::vacuum") {
                Some("calls vacuum::vacuum")
            } else if line.contains("auto_vacuum") || line.contains("incremental_vacuum") {
                Some("would make page reclamation implicit")
            } else {
                None
            };
            if let Some(reason) = reason {
                offenders.push(format!(
                    "{}:{} {reason}: {}",
                    file.display(),
                    index + 1,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        inspected_lines >= 2_000,
        "only {inspected_lines} executable lines inspected; the comment filter is \
         swallowing the whole crate"
    );
    assert!(
        offenders.is_empty(),
        "VACUUM must be reachable only through vacuum::vacuum, and page reclamation \
         must never be automatic:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn vacuum_sqlite_itself_rejects_the_statement_inside_a_transaction() {
    // The reason `vacuum` takes `&mut Connection`: a live `Transaction` holds
    // that borrow, so folding this into todo 82's single `IMMEDIATE` prune
    // transaction does not compile. This test proves the underlying constraint is
    // real in the SQLite this workspace links, so the signature is protecting
    // against a genuine failure rather than a remembered one.
    let mut fixture = Fixture::build();
    let transaction = fixture
        .connection
        .transaction()
        .expect("begin a transaction");
    let error = transaction
        .execute_batch("VACUUM")
        .expect_err("SQLite must reject VACUUM inside a transaction");
    assert!(
        error.to_string().to_lowercase().contains("transaction"),
        "{error}"
    );
    transaction.rollback().expect("roll back");

    let report = vacuum(&mut fixture.connection, &SystemDiskSpace)
        .expect("and it succeeds once no transaction is open");
    assert_eq!(report.freelist_pages_after, 0);
}

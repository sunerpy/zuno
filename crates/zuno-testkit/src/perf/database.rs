//! Read-only isolation and pinned-session selection for W-real.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

use crate::error::{Result, TestkitError};

use super::subject::{PinnedSubject, W_REAL_RECAPTURE, W_REAL_SUBJECT};

/// `rw-------`: one run's private, writable copy of the snapshot.
#[cfg(unix)]
const OWNER_READ_WRITE: u32 = 0o600;
/// `r--r--r--`: the shared snapshot no run may write back to.
#[cfg(unix)]
const READ_ONLY: u32 = 0o444;

/// Make `path` writable by its owner.
///
/// Split by platform because the Unix modes above are not expressible on Windows,
/// where the only file-permission bit `std` exposes is read-only. The Windows arm
/// clears that bit rather than doing nothing: this function's caller relies on the
/// clone being writable, and a silent no-op would turn a permission problem into a
/// confusing SQLite error later.
#[cfg(unix)]
fn set_owner_read_write(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(OWNER_READ_WRITE))
}

#[cfg(not(unix))]
fn set_owner_read_write(path: &Path) -> std::io::Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_readonly(false);
    std::fs::set_permissions(path, permissions)
}

/// Make `path` read-only, so no run can write back to the shared snapshot.
#[cfg(unix)]
fn set_read_only(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(READ_ONLY))
}

#[cfg(not(unix))]
fn set_read_only(path: &Path) -> std::io::Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(path, permissions)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RealSession {
    pub(crate) id: String,
    pub(crate) message_count: u64,
    pub(crate) part_count: u64,
    pub(crate) part_data_bytes: u64,
}

#[derive(Debug)]
pub(crate) struct RealDatabaseSnapshot {
    _root: TempDir,
    path: PathBuf,
    pub(crate) session: RealSession,
}

impl RealDatabaseSnapshot {
    pub(crate) fn capture() -> Result<Self> {
        let source = resolve_source_database()?;
        Self::capture_pinned(&source, &W_REAL_SUBJECT)
    }

    pub(crate) fn capture_from(source: &Path) -> Result<Self> {
        Self::capture_pinned(source, &W_REAL_SUBJECT)
    }

    /// Copy `source` and return the **pinned** session, or fail naming the pin.
    ///
    /// The database is verified against the pin before it is copied, because a
    /// 2.6 GB `.backup` of the wrong snapshot is minutes wasted on a run that
    /// cannot produce a comparable number.
    fn capture_pinned(source: &Path, pin: &PinnedSubject) -> Result<Self> {
        verify_pinned_database(source, pin)?;

        let root = tempfile::Builder::new()
            .prefix("zuno-testkit-real-db-")
            .tempdir()
            .map_err(|source| {
                TestkitError::io("create W-real snapshot directory", "<tempdir>", source)
            })?;
        let path = root.path().join("opencode.db");
        backup_sqlite_database(source, &path)?;
        make_sqlite_family_read_only(&path)?;
        let session = select_pinned_session(&path, pin)?;
        Ok(Self {
            _root: root,
            path,
            session,
        })
    }

    pub(crate) fn writable_clone(&self, target: &Path) -> Result<PathBuf> {
        std::fs::copy(&self.path, target).map_err(|source| {
            TestkitError::io("clone W-real snapshot for one run", target, source)
        })?;
        set_owner_read_write(target)
            .map_err(|source| TestkitError::io("make W-real run clone writable", target, source))?;
        Ok(target.to_path_buf())
    }
}

/// Where to *look* for the pinned snapshot.
///
/// The process environment locates a candidate file; it never decides what the
/// subject is. [`verify_database_identity`] is what makes a located file
/// acceptable, so a byte-identical copy at any path works and a mutated database
/// at the pinned path does not.
fn resolve_source_database() -> Result<PathBuf> {
    let layout = zuno_paths::Layout::from_process_env();
    let source = layout
        .db_path_for_channel("latest")
        .as_path()
        .map(Path::to_path_buf)
        .ok_or_else(|| TestkitError::RealDatabaseUnavailable {
            path: PathBuf::from(":memory:"),
            detail: format!(
                "the installed TypeScript release resolves ZUNO_DB to memory; point it at \
                 the pinned snapshot {}. {W_REAL_RECAPTURE}",
                W_REAL_SUBJECT.database_path
            ),
        })?;
    Ok(source)
}

/// Accept `source` only if it is byte-for-byte the pinned snapshot.
///
/// Public so the memory gate can reject an unpinned database in seconds rather
/// than after a release build and two measurement passes, without reimplementing
/// the comparison the capture path is tested through.
///
/// # Errors
/// Returns [`TestkitError::WRealDatabaseMismatch`] naming expected and found
/// identity, or a read/helper failure, always with the recapture procedure.
pub fn verify_pinned_database(source: &Path, pin: &PinnedSubject) -> Result<()> {
    let metadata =
        std::fs::metadata(source).map_err(|error| TestkitError::RealDatabaseUnavailable {
            path: source.to_path_buf(),
            detail: format!(
                "cannot read the W-real snapshot ({error}); the pin expects {} bytes with \
                 sha256 {}. {W_REAL_RECAPTURE}",
                pin.database_bytes, pin.database_sha256
            ),
        })?;
    if !metadata.is_file() {
        return Err(TestkitError::RealDatabaseUnavailable {
            path: source.to_path_buf(),
            detail: format!(
                "the resolved W-real path is not a file; the pin expects {} bytes with sha256 \
                 {}. {W_REAL_RECAPTURE}",
                pin.database_bytes, pin.database_sha256
            ),
        });
    }
    if metadata.len() != pin.database_bytes {
        return Err(TestkitError::WRealDatabaseMismatch {
            path: source.to_path_buf(),
            detail: format!(
                "expected {} bytes, found {}. {W_REAL_RECAPTURE}",
                pin.database_bytes,
                metadata.len()
            ),
        });
    }
    let digest = file_sha256(source)?;
    if digest != pin.database_sha256 {
        return Err(TestkitError::WRealDatabaseMismatch {
            path: source.to_path_buf(),
            detail: format!(
                "expected sha256 {}, found {digest}. {W_REAL_RECAPTURE}",
                pin.database_sha256
            ),
        });
    }
    Ok(())
}

fn file_sha256(path: &Path) -> Result<String> {
    let program = which::which("sha256sum").map_err(|_| TestkitError::HelperCommandNotFound {
        command: "sha256sum",
        remedy: "install coreutils; W-real verifies its snapshot against the committed pin",
    })?;
    let args = vec![path.to_string_lossy().into_owned()];
    let output = Command::new(&program)
        .args(&args)
        .output()
        .map_err(|source| TestkitError::Spawn {
            program: program.clone(),
            args: args.clone(),
            source,
        })?;
    if !output.status.success() {
        return Err(TestkitError::HelperCommandFailed {
            program,
            args,
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| TestkitError::RealDatabaseUnavailable {
            path: path.to_path_buf(),
            detail: "sha256sum produced no digest for the W-real snapshot".to_owned(),
        })
}

fn backup_sqlite_database(source: &Path, target: &Path) -> Result<()> {
    let sqlite = which::which("sqlite3").map_err(|_| TestkitError::HelperCommandNotFound {
        command: "sqlite3",
        remedy: "install sqlite3; W-real opens the user's source database read-only",
    })?;
    let escaped_target = target.to_string_lossy().replace('\'', "''");
    let args = vec![
        "-readonly".to_owned(),
        "-cmd".to_owned(),
        ".timeout 30000".to_owned(),
        source.to_string_lossy().into_owned(),
        format!(".backup '{escaped_target}'"),
    ];
    let output = Command::new(&sqlite)
        .args(&args)
        .output()
        .map_err(|source| TestkitError::Spawn {
            program: sqlite.clone(),
            args: args.clone(),
            source,
        })?;
    if !output.status.success() {
        return Err(TestkitError::HelperCommandFailed {
            program: sqlite,
            args,
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(())
}

fn make_sqlite_family_read_only(path: &Path) -> Result<()> {
    for candidate in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ] {
        if !candidate.exists() {
            continue;
        }
        set_read_only(&candidate).map_err(|source| {
            TestkitError::io("make W-real snapshot read-only", candidate, source)
        })?;
    }
    Ok(())
}

/// Read the pinned session, or fail loudly rather than substitute another.
///
/// The heaviest session is still queried — but only to describe what the database
/// *does* hold, so the failure tells an operator whether they are looking at the
/// wrong snapshot or need to re-pin. It never becomes the measured subject.
fn select_pinned_session(path: &Path, pin: &PinnedSubject) -> Result<RealSession> {
    let Some(found) = query_session(path, Some(pin.session_id))? else {
        let heaviest = query_session(path, None)?;
        let instead = heaviest.map_or_else(
            || "this database holds no session with part.data at all".to_owned(),
            |session| {
                format!(
                    "its heaviest session is {} with {} messages, {} parts and {} part bytes, \
                     which this gate will NOT silently measure in place of the pin",
                    session.id, session.message_count, session.part_count, session.part_data_bytes
                )
            },
        );
        return Err(TestkitError::WRealSubjectMissing {
            session_id: pin.session_id.to_owned(),
            path: path.to_path_buf(),
            detail: format!("{instead}. {W_REAL_RECAPTURE}"),
        });
    };
    let expected = RealSession {
        id: pin.session_id.to_owned(),
        message_count: pin.message_count,
        part_count: pin.part_count,
        part_data_bytes: pin.part_data_bytes,
    };
    if found != expected {
        return Err(TestkitError::WRealSubjectDrifted {
            session_id: pin.session_id.to_owned(),
            path: path.to_path_buf(),
            detail: format!(
                "expected {} messages, {} parts and {} part bytes; found {} messages, {} parts \
                 and {} part bytes. {W_REAL_RECAPTURE}",
                expected.message_count,
                expected.part_count,
                expected.part_data_bytes,
                found.message_count,
                found.part_count,
                found.part_data_bytes
            ),
        });
    }
    Ok(found)
}

/// Totals for one named session, or for the heaviest when `session` is `None`.
fn query_session(path: &Path, session: Option<&str>) -> Result<Option<RealSession>> {
    let sqlite = which::which("sqlite3").map_err(|_| TestkitError::HelperCommandNotFound {
        command: "sqlite3",
        remedy: "install sqlite3; W-real never opens the user's original database",
    })?;
    let filter = match session {
        Some(id) => format!("WHERE p.session_id = '{}' ", session_id_literal(id, path)?),
        None => String::new(),
    };
    let sql = format!(
        "SELECT p.session_id, COUNT(DISTINCT p.message_id), COUNT(*), SUM(LENGTH(p.data)) \
         FROM part AS p {filter}GROUP BY p.session_id \
         ORDER BY SUM(LENGTH(p.data)) DESC LIMIT 1;"
    );
    let args = vec![
        "-readonly".to_owned(),
        "-separator".to_owned(),
        "\t".to_owned(),
        path.to_string_lossy().into_owned(),
        sql,
    ];
    let output = Command::new(&sqlite)
        .args(&args)
        .output()
        .map_err(|source| TestkitError::Spawn {
            program: sqlite.clone(),
            args: args.clone(),
            source,
        })?;
    if !output.status.success() {
        return Err(TestkitError::HelperCommandFailed {
            program: sqlite,
            args,
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let row = stdout.trim();
    if row.is_empty() {
        return Ok(None);
    }
    let fields: Vec<&str> = row.split('\t').collect();
    if fields.len() != 4 || fields[0].is_empty() {
        return Err(TestkitError::RealDatabaseUnavailable {
            path: path.to_path_buf(),
            detail: format!("sqlite3 returned an unusable session row {row:?}"),
        });
    }
    let parse = |index: usize, name: &str| {
        fields[index]
            .parse::<u64>()
            .map_err(|source| TestkitError::ProcessTreeParse {
                pid: 0,
                path: path.to_path_buf(),
                value: format!("{name}={}", fields[index]),
                source,
            })
    };
    Ok(Some(RealSession {
        id: fields[0].to_owned(),
        message_count: parse(1, "message_count")?,
        part_count: parse(2, "part_count")?,
        part_data_bytes: parse(3, "part_data_bytes")?,
    }))
}

/// Accept a session id only if it cannot alter the statement it is spliced into.
///
/// The `sqlite3` CLI takes one SQL string with no bind parameters, so the id is
/// interpolated. Real ids are `ses_` plus base-62, and rejecting anything else
/// keeps a hand-edited pin from rewriting the query rather than failing it.
fn session_id_literal(id: &str, path: &Path) -> Result<String> {
    let acceptable = !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if acceptable {
        return Ok(id.to_owned());
    }
    Err(TestkitError::WRealSubjectMissing {
        session_id: id.to_owned(),
        path: path.to_path_buf(),
        detail: format!(
            "a pinned session id must be ASCII alphanumerics, `_` or `-`; this one cannot be \
             queried. {W_REAL_RECAPTURE}"
        ),
    })
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::process::Stdio;

    use super::*;

    /// One session in a fixture database, sized so its totals are exact.
    struct FixtureSession {
        id: &'static str,
        messages: u64,
        parts_per_message: u64,
        bytes_per_part: usize,
    }

    impl FixtureSession {
        fn part_count(&self) -> u64 {
            self.messages * self.parts_per_message
        }

        fn part_data_bytes(&self) -> u64 {
            self.part_count() * self.bytes_per_part as u64
        }
    }

    fn seed_fixture_database(path: &Path, sessions: &[FixtureSession]) {
        let mut script =
            String::from("CREATE TABLE part(session_id TEXT, message_id TEXT, data TEXT);\n");
        for session in sessions {
            let payload = "x".repeat(session.bytes_per_part);
            for message in 0..session.messages {
                for _ in 0..session.parts_per_message {
                    script.push_str(&format!(
                        "INSERT INTO part VALUES('{}','{}-m{message}','{payload}');\n",
                        session.id, session.id
                    ));
                }
            }
        }
        let sqlite = which::which("sqlite3").expect("sqlite3");
        let mut child = Command::new(sqlite)
            .arg(path)
            .stdin(Stdio::piped())
            .spawn()
            .expect("seed fixture database");
        child
            .stdin
            .take()
            .expect("sqlite stdin")
            .write_all(script.as_bytes())
            .expect("write fixture rows");
        assert!(
            child.wait().expect("join sqlite").success(),
            "seeding the fixture database failed"
        );
    }

    /// A pin describing `session` exactly as the fixture database holds it.
    ///
    /// The digest and path are leaked because [`PinnedSubject`] stores `'static`
    /// strings for the committed constant; a test process ends before it matters.
    fn pin_for(path: &Path, session: &FixtureSession) -> PinnedSubject {
        PinnedSubject {
            session_id: session.id,
            message_count: session.messages,
            part_count: session.part_count(),
            part_data_bytes: session.part_data_bytes(),
            database_path: path.to_string_lossy().into_owned().leak(),
            database_bytes: std::fs::metadata(path).expect("fixture metadata").len(),
            database_sha256: file_sha256(path).expect("fixture digest").leak(),
        }
    }

    fn subject() -> FixtureSession {
        FixtureSession {
            id: "ses_pinnedsubject",
            messages: 3,
            parts_per_message: 2,
            bytes_per_part: 16,
        }
    }

    /// Heavier than [`subject`], so "pinned" and "largest" cannot agree.
    fn heavier_decoy() -> FixtureSession {
        FixtureSession {
            id: "ses_heavierdecoy",
            messages: 5,
            parts_per_message: 4,
            bytes_per_part: 64,
        }
    }

    #[test]
    fn the_pinned_session_is_selected_twice_over_a_heavier_one() {
        // Given: a database whose heaviest session is not the pinned session.
        let root = tempfile::tempdir().expect("fixture directory");
        let path = root.path().join("fixture.db");
        seed_fixture_database(&path, &[subject(), heavier_decoy()]);
        let pin = pin_for(&path, &subject());

        // When: the snapshot is captured twice from the unchanged database.
        let first = RealDatabaseSnapshot::capture_pinned(&path, &pin).expect("first capture");
        let second = RealDatabaseSnapshot::capture_pinned(&path, &pin).expect("second capture");

        // Then: both captures return the pinned session with identical counts, and
        // neither returns the heavier one that a largest-session rule would pick.
        assert_eq!(first.session, second.session);
        assert_eq!(first.session.id, "ses_pinnedsubject");
        assert_eq!(first.session.message_count, 3);
        assert_eq!(first.session.part_count, 6);
        assert_eq!(first.session.part_data_bytes, 96);
        assert!(
            heavier_decoy().part_data_bytes() > first.session.part_data_bytes,
            "the decoy must be heavier or this test proves nothing"
        );
    }

    #[test]
    fn a_database_missing_the_pinned_session_fails_naming_the_recapture_procedure() {
        // Given: a database that holds only a heavier, unpinned session.
        let root = tempfile::tempdir().expect("fixture directory");
        let path = root.path().join("fixture.db");
        seed_fixture_database(&path, &[heavier_decoy()]);
        let pin = pin_for(&path, &subject());

        // When: the pinned subject is requested.
        let error = RealDatabaseSnapshot::capture_pinned(&path, &pin)
            .expect_err("an absent pinned session must fail");

        // Then: the failure names the pin, states that the heavier session present
        // will not be substituted for it, and prints the recapture procedure.
        let message = error.to_string();
        assert!(
            matches!(error, TestkitError::WRealSubjectMissing { .. }),
            "{message}"
        );
        assert!(message.contains("ses_pinnedsubject"), "{message}");
        assert!(message.contains("ses_heavierdecoy"), "{message}");
        assert!(message.contains("1280"), "{message}");
        assert!(message.contains("will NOT silently measure"), "{message}");
        assert!(message.contains("W_REAL_SUBJECT"), "{message}");
        assert!(message.contains("benchmarks/ts-baseline.json"), "{message}");
    }

    #[test]
    fn a_pinned_session_whose_content_drifted_is_not_measured() {
        // Given: a database holding the pinned session with more parts than pinned.
        let root = tempfile::tempdir().expect("fixture directory");
        let path = root.path().join("fixture.db");
        seed_fixture_database(
            &path,
            &[FixtureSession {
                parts_per_message: 3,
                ..subject()
            }],
        );
        let pin = pin_for(&path, &subject());

        // When: the pinned subject is requested.
        let error = RealDatabaseSnapshot::capture_pinned(&path, &pin)
            .expect_err("a drifted pinned session must fail");

        // Then: the failure reports expected and found counts side by side rather
        // than measuring a session that no longer matches the committed ceiling.
        let message = error.to_string();
        assert!(
            matches!(error, TestkitError::WRealSubjectDrifted { .. }),
            "{message}"
        );
        assert!(
            message.contains("expected 3 messages, 6 parts and 96 part bytes"),
            "{message}"
        );
        assert!(
            message.contains("found 3 messages, 9 parts and 144 part bytes"),
            "{message}"
        );
        assert!(message.contains("W_REAL_SUBJECT"), "{message}");
    }

    #[test]
    fn a_database_that_is_not_the_pinned_snapshot_is_rejected_before_it_is_copied() {
        // Given: a database that holds the pinned session, and two pins that
        // disagree with its identity in each of the two checked ways.
        let root = tempfile::tempdir().expect("fixture directory");
        let path = root.path().join("fixture.db");
        seed_fixture_database(&path, &[subject()]);
        let honest = pin_for(&path, &subject());
        let wrong_length = PinnedSubject {
            database_bytes: honest.database_bytes + 1,
            ..honest
        };
        let wrong_digest = PinnedSubject {
            database_sha256: "0000000000000000000000000000000000000000000000000000000000000000",
            ..honest
        };

        // When: each is used to capture the snapshot.
        let by_length = RealDatabaseSnapshot::capture_pinned(&path, &wrong_length)
            .expect_err("a differently sized database must fail");
        let by_digest = RealDatabaseSnapshot::capture_pinned(&path, &wrong_digest)
            .expect_err("a differently hashed database must fail");

        // Then: both fail as identity mismatches naming expected and found, so the
        // session fingerprint alone cannot admit an unpinned database.
        assert!(
            matches!(by_length, TestkitError::WRealDatabaseMismatch { .. }),
            "{by_length}"
        );
        assert!(
            matches!(by_digest, TestkitError::WRealDatabaseMismatch { .. }),
            "{by_digest}"
        );
        assert!(
            by_length
                .to_string()
                .contains(&format!("expected {} bytes", honest.database_bytes + 1)),
            "{by_length}"
        );
        assert!(
            by_digest.to_string().contains(honest.database_sha256),
            "{by_digest}"
        );
        assert!(
            by_digest.to_string().contains("0000000000000000"),
            "{by_digest}"
        );

        // And: the honest pin still succeeds, so the checks are not unconditional.
        RealDatabaseSnapshot::capture_pinned(&path, &honest).expect("the honest pin must capture");
    }

    #[test]
    fn a_session_id_that_could_rewrite_the_query_is_rejected_rather_than_interpolated() {
        // Given: a fixture database and a pin whose id carries SQL punctuation.
        let root = tempfile::tempdir().expect("fixture directory");
        let path = root.path().join("fixture.db");
        seed_fixture_database(&path, &[subject()]);
        let honest = pin_for(&path, &subject());
        let hostile = PinnedSubject {
            session_id: "ses_x'; DROP TABLE part; --",
            ..honest
        };

        // When: the hostile pin is used.
        let error = RealDatabaseSnapshot::capture_pinned(&path, &hostile)
            .expect_err("an unquotable session id must fail");

        // Then: it fails as a missing subject, and the table it tried to drop is
        // still there, so the id never reached the statement.
        assert!(
            matches!(error, TestkitError::WRealSubjectMissing { .. }),
            "{error}"
        );
        let surviving = query_session(&path, Some("ses_pinnedsubject"))
            .expect("query the surviving table")
            .expect("the pinned session must still exist");
        assert_eq!(surviving.part_count, 6);
    }

    #[test]
    fn snapshot_contains_wal_rows_without_sidecars() {
        // Given: a live WAL database whose committed row is not checkpointed.
        let root = tempfile::tempdir().expect("temporary database directory");
        let source = root.path().join("source.db");
        let target = root.path().join("target.db");
        let sqlite = which::which("sqlite3").expect("sqlite3");
        let mut writer = Command::new(&sqlite)
            .arg(&source)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("start sqlite writer");
        let mut stdin = writer.stdin.take().expect("sqlite stdin");
        let stdout = writer.stdout.take().expect("sqlite stdout");
        stdin
            .write_all(
                b"PRAGMA journal_mode=WAL;\nPRAGMA wal_autocheckpoint=0;\n\
                  CREATE TABLE probe(value TEXT);\nINSERT INTO probe VALUES('visible');\n.print READY\n",
            )
            .expect("seed live WAL database");
        stdin.flush().expect("flush sqlite commands");
        let mut output = BufReader::new(stdout);
        let mut line = String::new();
        while output.read_line(&mut line).expect("read sqlite output") > 0 {
            if line.trim() == "READY" {
                break;
            }
            line.clear();
        }

        // When: the live database is captured and all snapshot sidecars are removed.
        backup_sqlite_database(&source, &target).expect("capture live database");
        for suffix in ["-wal", "-shm"] {
            let sidecar = PathBuf::from(format!("{}{suffix}", target.display()));
            let _ = std::fs::remove_file(sidecar);
        }
        stdin.write_all(b".quit\n").expect("stop sqlite writer");
        drop(stdin);
        writer.wait().expect("join sqlite writer");

        // Then: the standalone snapshot contains the committed WAL row.
        let result = Command::new(sqlite)
            .args([
                target.to_string_lossy().as_ref(),
                "SELECT COUNT(*) FROM probe;",
            ])
            .output()
            .expect("query snapshot");
        assert!(result.status.success(), "{:?}", result.stderr);
        assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "1");
    }
}

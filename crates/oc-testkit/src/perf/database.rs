//! Read-only isolation and largest-session selection for W-real.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

use crate::error::{Result, TestkitError};

/// `rw-------`: one run's private, writable copy of the snapshot.
const OWNER_READ_WRITE: u32 = 0o600;
/// `r--r--r--`: the shared snapshot no run may write back to.
const READ_ONLY: u32 = 0o444;

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
        let layout = oc_paths::Layout::from_process_env();
        let source = layout
            .db_path_for_channel("latest")
            .as_path()
            .map(Path::to_path_buf)
            .ok_or_else(|| TestkitError::RealDatabaseUnavailable {
                path: PathBuf::from(":memory:"),
                detail: "the installed TypeScript release resolves OPENCODE_DB to memory"
                    .to_owned(),
            })?;
        if !source.is_file() {
            return Err(TestkitError::RealDatabaseUnavailable {
                path: source,
                detail: "build a real session history with the TypeScript opencode CLI first"
                    .to_owned(),
            });
        }

        let root = tempfile::Builder::new()
            .prefix("oc-testkit-real-db-")
            .tempdir()
            .map_err(|source| {
                TestkitError::io("create W-real snapshot directory", "<tempdir>", source)
            })?;
        let path = root.path().join("opencode.db");
        backup_sqlite_database(&source, &path)?;
        make_sqlite_family_read_only(&path)?;
        let session = select_largest_session(&path)?;
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
        std::fs::set_permissions(target, std::fs::Permissions::from_mode(OWNER_READ_WRITE))
            .map_err(|source| TestkitError::io("make W-real run clone writable", target, source))?;
        Ok(target.to_path_buf())
    }
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
        std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(READ_ONLY)).map_err(
            |source| TestkitError::io("make W-real snapshot read-only", candidate, source),
        )?;
    }
    Ok(())
}

fn select_largest_session(path: &Path) -> Result<RealSession> {
    let sqlite = which::which("sqlite3").map_err(|_| TestkitError::HelperCommandNotFound {
        command: "sqlite3",
        remedy: "install sqlite3; W-real never opens the user's original database",
    })?;
    let sql = concat!(
        "SELECT p.session_id, COUNT(DISTINCT p.message_id), COUNT(*), ",
        "SUM(LENGTH(p.data)) FROM part AS p GROUP BY p.session_id ",
        "ORDER BY SUM(LENGTH(p.data)) DESC LIMIT 1;"
    );
    let args = vec![
        "-readonly".to_owned(),
        "-separator".to_owned(),
        "\t".to_owned(),
        path.to_string_lossy().into_owned(),
        sql.to_owned(),
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
    let fields: Vec<&str> = stdout.trim().split('\t').collect();
    if fields.len() != 4 || fields[0].is_empty() {
        return Err(TestkitError::RealDatabaseUnavailable {
            path: path.to_path_buf(),
            detail: "the copied database contains no session with part.data".to_owned(),
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
    Ok(RealSession {
        id: fields[0].to_owned(),
        message_count: parse(1, "message_count")?,
        part_count: parse(2, "part_count")?,
        part_data_bytes: parse(3, "part_data_bytes")?,
    })
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::process::Stdio;

    use super::*;

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

//! `export` and `import`: one session's transcript out to JSON and back in.
//!
//! Both handlers are deliberately thin. The envelope, the ordering, the
//! redaction pass and the row writes all live in [`zuno_db::session_export`], so
//! this module owns only what a CLI owns: which database to open, where the bytes
//! go, and what a failure reads like.
//!
//! # stdout carries the document and nothing else
//!
//! `export` prints the JSON to stdout and its progress line to stderr, matching
//! `cli/cmd/export.ts:243` and `:289`. That separation is the whole point of the
//! command: `zuno export ses_… > backup.json` has to produce a file that
//! parses, so a status line on stdout would corrupt every backup.

use std::path::Path;

use zuno_db::session_export::{self, ImportTarget};

use crate::command::{ExportArgs, ImportArgs};

/// Print one session's whole transcript as JSON.
pub(super) fn export(args: &ExportArgs) -> Result<(), String> {
    let session_id = args.session_id.as_deref().ok_or(
        "session selection is interactive upstream; pass the session id, \
         which `session list` prints",
    )?;
    let pool = open_pool()?;
    let connection = pool.get().map_err(to_string)?;

    eprintln!("Exporting session: {session_id}");
    let document = session_export::export(&connection, session_id)
        .map_err(|_| format!("Session not found: {session_id}"))?
        .to_json()
        .map_err(to_string)?;
    let document = if args.sanitize {
        session_export::sanitize(document)
    } else {
        document
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&document).map_err(to_string)?
    );
    Ok(())
}

/// Read an exported document back into this checkout's database.
pub(super) fn import(args: &ImportArgs) -> Result<(), String> {
    // Upstream also accepts a share URL (`cli/cmd/import.ts:117-160`). Zuno does
    // not integrate with that hosted service, and letting the URL fall through
    // to the file reader would report `File not found` for a URL that is perfectly
    // well formed.
    if args.file.starts_with("http://") || args.file.starts_with("https://") {
        return Err(format!(
            "cannot import from {}: Zuno does not integrate with the hosted share service; \
             export the session with the hosted client and import the resulting file instead",
            args.file
        ));
    }
    let path = Path::new(&args.file);
    let raw = std::fs::read_to_string(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => format!("File not found: {}", args.file),
        std::io::ErrorKind::PermissionDenied => {
            String::from("Failed to read file: Permission denied")
        }
        _ => format!("Failed to read file: {error}"),
    })?;
    let document: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|error| format!("Invalid JSON in {}: {error}", args.file))?;

    let directory = std::env::current_dir().map_err(to_string)?;
    let project = zuno_paths::project::resolve_project(&directory);
    let worktree = project
        .vcs
        .as_ref()
        .map_or(directory.as_path(), |_| project.directory.as_path());
    let target = ImportTarget {
        project_id: project.id.clone(),
        directory: directory.to_string_lossy().into_owned(),
        path: zuno_db::session::session_path(worktree, &directory),
    };

    let pool = open_pool()?;
    let imported = pool
        .transaction(|transaction| {
            write_project(transaction, &project)?;
            session_export::import(transaction, &document, &target)
        })
        .map_err(to_string)?;

    println!("Imported session: {}", imported.session_id);
    Ok(())
}

/// Ensure the project row the imported session will reference exists.
///
/// The session's `project_id` is a cascading foreign key, so an import into a
/// checkout this database has never seen would otherwise fail on the constraint
/// rather than on anything the user did.
fn write_project(
    transaction: &rusqlite::Transaction<'_>,
    project: &zuno_paths::project::ResolvedProject,
) -> Result<(), zuno_error::DbError> {
    let now = zuno_db::message::now_millis();
    transaction
        .execute(
            "INSERT INTO project (id, worktree, vcs, time_created, time_updated, sandboxes) \
             VALUES (?1, ?2, ?3, ?4, ?4, '[]') \
             ON CONFLICT (id) DO UPDATE SET \
               worktree = excluded.worktree, \
               time_updated = excluded.time_updated",
            (
                project.id.as_str(),
                project.directory.to_string_lossy().as_ref(),
                project.vcs.as_ref().map(|_| "git"),
                now,
            ),
        )
        .map_err(|source| zuno_error::DbError::Query {
            source: Box::new(source),
        })?;
    Ok(())
}

fn open_pool() -> Result<zuno_db::Pool, String> {
    let pool = zuno_db::Pool::open_default().map_err(to_string)?;
    let mut connection = pool.get().map_err(to_string)?;
    zuno_db::migration::apply(&mut connection).map_err(to_string)?;
    drop(connection);
    Ok(pool)
}

fn to_string(error: impl std::fmt::Display) -> String {
    error.to_string()
}

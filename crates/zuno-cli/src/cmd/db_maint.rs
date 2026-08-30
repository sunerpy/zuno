//! `db stats`, `db integrity-check`, and `db vacuum` — the maintenance half of
//! the `db` command.
//!
//! # Why these are three commands and not one flag
//!
//! Upstream's `db` is a query runner and a path printer
//! (`packages/opencode/src/cli/cmd/db.ts:8-62`), and nothing anywhere in the
//! TypeScript tree ever runs `VACUUM`. So a deleted session's pages stay in the
//! file forever and there is no way to see how large the database has become.
//! These three answer, in order: how big is it and what is making it big, is it
//! intact, and reclaim the space — the last one only when asked, because
//! `VACUUM` rewrites the whole file. [`zuno_db::vacuum`] documents why that must
//! never be a side effect of a prune.
//!
//! # Why they are positional keywords rather than clap subcommands
//!
//! `db path` was already dispatched on the value of the `query` positional, for a
//! reason worth preserving: `crates/zuno-cli/tests/differential.rs` compares
//! `db --help`'s **long options** against the real binary's, and the comparison is
//! exact unless an addition is declared. A clap subcommand alongside a positional
//! would also need `args_conflicts_with_subcommands`, turning one unambiguous
//! string match into a parser configuration nobody can read. Adding keywords adds
//! no flag, so the frozen surface stays frozen. No `SELECT` is spelled `stats`,
//! and upstream's own `db stats` does not exist, so nothing that worked before
//! changes meaning.

use zuno_db::vacuum::{
    Availability, DEFAULT_LARGEST_SESSIONS, DatabaseStats, IntegrityReport, SystemDiskSpace,
    VacuumReport, format_bytes, integrity_check, stats, to_json, vacuum,
};

use crate::command::DbFormat;

/// The maintenance operations reachable through the `db` positional.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Maintenance {
    /// Rows per table, on-disk size, WAL size, and the heaviest sessions.
    Stats,
    /// `PRAGMA integrity_check` plus `PRAGMA foreign_key_check`.
    IntegrityCheck,
    /// An explicit whole-file rewrite, reporting the bytes reclaimed.
    Vacuum,
}

impl Maintenance {
    /// Recognize a maintenance keyword, or `None` for anything to run as SQL.
    pub(super) fn parse(query: &str) -> Option<Self> {
        match query {
            "stats" => Some(Self::Stats),
            // Both spellings: the command reads as two words and the flag-style
            // hyphen is what a user types, while `integrity_check` is what the
            // pragma is called and therefore what someone reading the output
            // reaches for.
            "integrity-check" | "integrity_check" => Some(Self::IntegrityCheck),
            "vacuum" => Some(Self::Vacuum),
            _ => None,
        }
    }
}

/// Run one maintenance command against the database this binary would use.
///
/// # Errors
///
/// A message naming the failure: a database that cannot be opened or migrated, a
/// refusal from the disk guard, or an integrity check that found damage. An
/// integrity failure is an error rather than a report so a script's exit status
/// means what it looks like it means.
pub(super) fn execute(command: Maintenance, format: DbFormat) -> Result<(), String> {
    let mut connection = zuno_db::open_default().map_err(|error| error.to_string())?;
    zuno_db::migration::apply(&mut connection).map_err(|error| error.to_string())?;

    match command {
        Maintenance::Stats => {
            let summary =
                stats(&connection, DEFAULT_LARGEST_SESSIONS).map_err(|error| error.to_string())?;
            emit(&summary, format, || render_stats(&summary))
        }
        Maintenance::IntegrityCheck => {
            let report = integrity_check(&connection).map_err(|error| error.to_string())?;
            emit(&report, format, || render_integrity(&report))?;
            if report.is_ok() {
                Ok(())
            } else {
                Err(format!(
                    "integrity check failed: {} structural problem(s) and {} dangling \
                     reference(s); the database is damaged and a vacuum will not repair it",
                    report
                        .integrity
                        .iter()
                        .filter(|line| line.as_str() != zuno_db::vacuum::INTEGRITY_OK)
                        .count(),
                    report.foreign_key_violations.len(),
                ))
            }
        }
        Maintenance::Vacuum => {
            let report =
                vacuum(&mut connection, &SystemDiskSpace).map_err(|error| error.to_string())?;
            if report.fts_rebuild_required {
                zuno_db::fts::rebuild(&connection).map_err(|error| error.to_string())?;
            }
            emit(&report, format, || render_vacuum(&report))
        }
    }
}

fn emit<T: serde::Serialize>(
    report: &T,
    format: DbFormat,
    text: impl FnOnce() -> String,
) -> Result<(), String> {
    match format {
        DbFormat::Json => {
            let json = to_json(report).map_err(|error| error.to_string())?;
            let rendered =
                serde_json::to_string_pretty(&json).map_err(|error| error.to_string())?;
            println!("{rendered}");
        }
        DbFormat::Tsv => println!("{}", text()),
    }
    Ok(())
}

fn render_stats(summary: &DatabaseStats) -> String {
    let mut lines = vec![format!(
        "database\t{}",
        summary
            .path
            .as_ref()
            .map_or_else(|| ":memory:".to_owned(), |path| path.display().to_string())
    )];
    for (label, bytes) in [
        ("file", summary.size.main_bytes),
        ("wal", summary.size.wal_bytes),
        ("shm", summary.size.shm_bytes),
        ("total", summary.size.total_bytes()),
    ] {
        lines.push(format!("{label}\t{}\t{bytes} bytes", format_bytes(bytes)));
    }
    lines.push(format!(
        "pages\t{} x {} bytes\t{} reclaimable by `db vacuum`",
        summary.page_count, summary.page_size, summary.freelist_pages
    ));
    lines.push(String::new());
    lines.push("table\trows".to_owned());
    for entry in &summary.tables {
        lines.push(format!("{}\t{}", entry.table, entry.rows));
    }
    lines.push(format!("TOTAL\t{}", summary.total_rows));

    if !summary.largest_sessions.is_empty() {
        lines.push(String::new());
        lines.push("session\tparts\tbytes\ttitle".to_owned());
        for session in &summary.largest_sessions {
            lines.push(format!(
                "{}\t{}\t{}\t{}",
                session.session_id,
                session.part_rows,
                format_bytes(session.part_bytes),
                session.title
            ));
        }
    }
    lines.join("\n")
}

fn render_integrity(report: &IntegrityReport) -> String {
    let mut lines = Vec::new();
    for line in &report.integrity {
        lines.push(format!("integrity_check\t{line}"));
    }
    if report.foreign_key_violations.is_empty() {
        lines.push("foreign_key_check\tok".to_owned());
    } else {
        for violation in &report.foreign_key_violations {
            lines.push(format!(
                "foreign_key_check\t{}\trowid={}\tparent={}\tfk={}",
                violation.table,
                violation
                    .rowid
                    .map_or_else(|| "-".to_owned(), |rowid| rowid.to_string()),
                violation.parent,
                violation.foreign_key_index
            ));
        }
    }
    lines.join("\n")
}

fn render_vacuum(report: &VacuumReport) -> String {
    let mut lines = vec![
        format!("database\t{}", report.path.display()),
        format!(
            "before\t{}\t{} bytes",
            format_bytes(report.before.total_bytes()),
            report.before.total_bytes()
        ),
        format!(
            "after\t{}\t{} bytes",
            format_bytes(report.after.total_bytes()),
            report.after.total_bytes()
        ),
        format!(
            "reclaimed\t{}\t{} bytes",
            format_bytes(report.reclaimed_bytes),
            report.reclaimed_bytes
        ),
        format!(
            "freelist\t{} pages -> {} pages",
            report.freelist_pages_before, report.freelist_pages_after
        ),
    ];
    lines.push(match &report.available_bytes {
        Availability::Known { bytes } => format!(
            "free space\t{}\t{bytes} bytes available before the rewrite",
            format_bytes(*bytes)
        ),
        Availability::Unknown { reason } => {
            format!("free space\tunknown\t{reason}")
        }
    });
    if report.fts_rebuild_required {
        lines.push("search index\trebuilt after the rewrite".to_owned());
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_maintenance_keyword_is_recognized_and_nothing_else_is() {
        assert_eq!(Maintenance::parse("stats"), Some(Maintenance::Stats));
        assert_eq!(
            Maintenance::parse("integrity-check"),
            Some(Maintenance::IntegrityCheck)
        );
        assert_eq!(
            Maintenance::parse("integrity_check"),
            Some(Maintenance::IntegrityCheck)
        );
        assert_eq!(Maintenance::parse("vacuum"), Some(Maintenance::Vacuum));

        // A query must never be swallowed by keyword matching, and the match is
        // exact: an upstream `db path` still reaches its own branch, and the
        // uppercase SQL keyword is not a maintenance command.
        for query in [
            "path",
            "VACUUM",
            "Stats",
            "SELECT 1",
            "select count(*) from session",
            "",
            " stats",
            "stats;",
        ] {
            assert_eq!(Maintenance::parse(query), None, "{query} must run as SQL");
        }
    }
}

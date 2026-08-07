//! `session list` — the cross-project listing, its flags, and its two output
//! forms.
//!
//! # What this fixes
//!
//! Upstream's `session list` calls `svc.list({ roots: true, limit })`
//! (`cli/cmd/session.ts:87`), and `Session.list` injects the ambient project id
//! into every query with no input that turns it off (`session/session.ts:548-555`,
//! `:957-965`). The result is a CLI that can only ever show the checkout you are
//! standing in, while the data and the `/experimental/session` endpoint
//! (`server/routes/instance/httpapi/groups/experimental.ts:224-233`) have always
//! spanned projects. Nothing is injected here: the scope comes from the flags,
//! and [`crate::command::SessionListArgs`] defaults it to the current project so
//! the no-flag invocation still behaves the way it always has.
//!
//! # Two declared divergences from upstream's CLI
//!
//! * `--format json` emits the endpoint's `GlobalInfo` array, not
//!   `formatSessionJSON`'s six flat fields (`cli/cmd/session.ts:137-147`). That
//!   shape spells the project id `projectId`, which neither the endpoint
//!   (`projectID`) nor the database (`project_id`) uses, and it drops the project
//!   summary that makes a cross-project listing readable. One shape, and it is
//!   the one the documented endpoint already publishes.
//! * There is no pager. Upstream shells out to `less` when stdout is a TTY
//!   (`cli/cmd/session.ts:93-111`); piping to a pager from a listing command
//!   means the caller cannot compose it, and `| less` is one keystroke.

use oc_db::session_list::{
    GlobalListRequest, ListedSession, ProjectInfo, ProjectScope, resolve_project,
};
use oc_db::{Pool, session_list};
use time::{OffsetDateTime, UtcOffset};

use crate::command::{SessionFormat, SessionListArgs, SessionSortKey};

/// Widths for the six required columns plus the session id.
///
/// Fixed rather than fitted to the data. A width that follows the longest title
/// makes every run a different shape, so two listings cannot be diffed and a
/// terminal that fit yesterday wraps today. The id keeps its natural width
/// because it is the column a caller copies into `session delete`, and a
/// truncated id is useless.
const PROJECT_WIDTH: usize = 18;
const TITLE_WIDTH: usize = 32;
const AGENT_WIDTH: usize = 9;
/// Wide enough for the longest stamp `today_time_or_date_time` can produce
/// (`11:38 PM · 12/31/2026`, 21 units) **plus** ` (archived)`. Sized for the
/// worst case on purpose: at 20 the marker was truncated to a bare `…`, which
/// told the reader a cell had been cut but not that the session was archived —
/// the one thing `--archived` exists to show.
const ACTIVITY_WIDTH: usize = 32;
const MESSAGES_WIDTH: usize = 4;
const COST_WIDTH: usize = 8;
const ID_WIDTH: usize = 20;

/// Run `session list`.
///
/// # Errors
///
/// A message naming the failure: an unresolvable `--project`, a database that
/// cannot be read, or a clock the activity column cannot format.
pub(super) fn run(pool: &Pool, args: &SessionListArgs) -> Result<(), String> {
    let connection = pool.get().map_err(|error| error.to_string())?;
    let request = GlobalListRequest {
        scope: scope(&connection, args)?,
        roots: args.roots_only(),
        archived: args.archived,
        sort: match args.sort {
            SessionSortKey::Updated => oc_db::session::SessionSort::Updated,
            SessionSortKey::Created => oc_db::session::SessionSort::Created,
        },
        limit: args.limit,
        search: None,
    };
    let listed = session_list::list(&connection, &request).map_err(|error| error.to_string())?;

    match args.format {
        SessionFormat::Json => {
            let json = session_list::to_json(&listed).map_err(|error| error.to_string())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json).map_err(|error| error.to_string())?
            );
            Ok(())
        }
        SessionFormat::Table => render_table(&listed),
    }
}

/// Which projects the flags select.
///
/// With neither flag the current checkout wins, which is what upstream's
/// injection produced — the difference is that this is a resolved default a
/// caller can override rather than a hidden predicate.
fn scope(connection: &oc_db::Connection, args: &SessionListArgs) -> Result<ProjectScope, String> {
    if args.all_projects {
        return Ok(ProjectScope::AllProjects);
    }
    if let Some(needle) = &args.project {
        let resolved = resolve(connection, needle)?;
        return Ok(ProjectScope::Project(resolved.id));
    }
    let directory = std::env::current_dir().map_err(|error| error.to_string())?;
    Ok(ProjectScope::Project(
        oc_paths::project::resolve_project(&directory).id,
    ))
}

/// Resolve `--project` against the `project` table, by id or by worktree path.
///
/// A path is canonicalised first so `.`, `..` and a symlinked checkout all reach
/// the row the worktree column holds; the raw string is still tried, because a
/// project id is not a path and a worktree that has since been deleted cannot be
/// canonicalised but is still listable.
fn resolve(connection: &oc_db::Connection, needle: &str) -> Result<ProjectInfo, String> {
    let canonical = std::fs::canonicalize(needle)
        .ok()
        .map(|path| path.to_string_lossy().into_owned());
    for candidate in [Some(needle.to_owned()), canonical].into_iter().flatten() {
        if let Some(found) =
            resolve_project(connection, &candidate).map_err(|error| error.to_string())?
        {
            return Ok(found);
        }
    }
    Err(format!(
        "Project not found: {needle} (expected a project id or a worktree path; \
         `session list --all-projects` shows every project)"
    ))
}

fn render_table(listed: &[ListedSession]) -> Result<(), String> {
    if listed.is_empty() {
        return Ok(());
    }
    let id_width = listed
        .iter()
        .map(|entry| entry.info.id.len())
        .max()
        .unwrap_or(ID_WIDTH)
        .max(ID_WIDTH);

    let header = format!(
        "{:<id_width$}  {:<PROJECT_WIDTH$}  {:<TITLE_WIDTH$}  {:<AGENT_WIDTH$}  \
         {:<ACTIVITY_WIDTH$}  {:>MESSAGES_WIDTH$}  {:>COST_WIDTH$}",
        "Session ID", "Project", "Title", "Agent", "Last activity", "Msgs", "Cost",
    );
    println!("{header}");
    println!("{}", "─".repeat(header.chars().count()));

    for entry in listed {
        println!(
            "{:<id_width$}  {:<PROJECT_WIDTH$}  {:<TITLE_WIDTH$}  {:<AGENT_WIDTH$}  \
             {:<ACTIVITY_WIDTH$}  {:>MESSAGES_WIDTH$}  {:>COST_WIDTH$}",
            entry.info.id,
            truncate(&project_label(entry), PROJECT_WIDTH),
            truncate(&entry.info.title, TITLE_WIDTH),
            truncate(entry.info.agent.as_deref().unwrap_or("-"), AGENT_WIDTH),
            activity(entry)?,
            entry.messages,
            cost(entry.info.cost),
        );
    }
    Ok(())
}

/// How a project is named in the table.
///
/// The display name when the project set one, otherwise the worktree's last
/// component — the name a developer would use for the checkout — and the raw
/// project id when the project row is gone entirely, because the id is the only
/// truth left on the session row and inventing a placeholder would hide the
/// dangling reference.
fn project_label(entry: &ListedSession) -> String {
    match &entry.info.project {
        Some(project) => project.name.clone().unwrap_or_else(|| {
            std::path::Path::new(&project.worktree)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| project.id.clone())
        }),
        None => entry.info.project_id.clone(),
    }
}

/// The cost cell, as a `String` rather than `format_args!`.
///
/// `format_args!` inside a padded slot is silently unpadded — the outer width
/// applies to the argument, and a lazily formatted one has no width to apply it
/// to. The column looked right only because every value happened to be the same
/// length; clippy's `unused_format_specs` caught it.
fn cost(dollars: f64) -> String {
    format!("${dollars:.2}")
}

/// The last-activity cell: whichever timestamp the listing sorted on, with an
/// archive marker when the row is one `--archived` widened the result with.
fn activity(entry: &ListedSession) -> Result<String, String> {
    let stamp = today_time_or_date_time(entry.info.time.updated)?;
    if entry.info.time.archived.is_some() {
        return Ok(truncate(&format!("{stamp} (archived)"), ACTIVITY_WIDTH));
    }
    Ok(stamp)
}

/// `Locale.todayTimeOrDateTime` (`util/locale.ts`): a bare clock today, clock
/// plus date otherwise.
fn today_time_or_date_time(milliseconds: i64) -> Result<String, String> {
    let seconds = milliseconds.div_euclid(1_000);
    let nanos = u32::try_from(milliseconds.rem_euclid(1_000)).map_err(|error| error.to_string())?
        * 1_000_000;
    let utc = OffsetDateTime::from_unix_timestamp(seconds)
        .map_err(|error| error.to_string())?
        .replace_nanosecond(nanos)
        .map_err(|error| error.to_string())?;
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let date = utc.to_offset(offset);
    let now = OffsetDateTime::now_utc().to_offset(offset);
    let hour = match date.hour() % 12 {
        0 => 12,
        value => value,
    };
    let suffix = if date.hour() < 12 { "AM" } else { "PM" };
    let clock = format!("{hour}:{} {suffix}", format_args!("{:02}", date.minute()));
    if date.date() == now.date() {
        Ok(clock)
    } else {
        Ok(format!(
            "{clock} · {}/{}/{}",
            u8::from(date.month()),
            date.day(),
            date.year()
        ))
    }
}

/// Cut a cell to `limit` **UTF-16 units**, ending in `…` when it was cut.
///
/// UTF-16 rather than characters or bytes because upstream measures in
/// JavaScript string length (`Locale.truncate`), and a column that agrees with
/// the TypeScript binary on ASCII but disagrees on an emoji is worse than one
/// that is consistently wrong.
fn truncate(value: &str, limit: usize) -> String {
    if utf16_len(value) <= limit {
        return value.to_owned();
    }
    let keep = limit.saturating_sub(1);
    let mut units = 0;
    let mut end = 0;
    for (index, character) in value.char_indices() {
        let next = units + character.len_utf16();
        if next > keep {
            break;
        }
        units = next;
        end = index + character.len_utf8();
    }
    format!("{}…", &value[..end])
}

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_db::session_list::{CacheUsage, GlobalInfo, TimeInfo, TokenUsage};

    fn info(id: &str, project: Option<ProjectInfo>) -> GlobalInfo {
        GlobalInfo {
            id: id.to_owned(),
            slug: id.to_owned(),
            project_id: String::from("prj_one"),
            workspace_id: None,
            directory: String::from("/srv/one"),
            path: Some(String::new()),
            parent_id: None,
            title: String::from("A title"),
            agent: Some(String::from("build")),
            model: None,
            version: String::from("1.18.13"),
            summary: None,
            cost: 0.0,
            tokens: TokenUsage {
                input: 0,
                output: 0,
                reasoning: 0,
                cache: CacheUsage { read: 0, write: 0 },
            },
            share: None,
            metadata: None,
            revert: None,
            permission: None,
            time: TimeInfo {
                created: 0,
                updated: 0,
                compacting: None,
                archived: None,
            },
            project,
        }
    }

    fn entry(project: Option<ProjectInfo>) -> ListedSession {
        ListedSession {
            info: info("ses_one", project),
            messages: 0,
        }
    }

    #[test]
    fn truncation_uses_javascript_utf16_width() {
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("a𝄞bc", 4), "a𝄞…");
    }

    #[test]
    fn a_named_project_wins_over_its_worktree() {
        let label = project_label(&entry(Some(ProjectInfo {
            id: String::from("prj_one"),
            name: Some(String::from("One")),
            worktree: String::from("/srv/checkout"),
        })));
        assert_eq!(label, "One");
    }

    #[test]
    fn an_unnamed_project_shows_its_worktree_basename() {
        let label = project_label(&entry(Some(ProjectInfo {
            id: String::from("prj_one"),
            name: None,
            worktree: String::from("/srv/checkout"),
        })));
        assert_eq!(label, "checkout");
    }

    #[test]
    fn a_missing_project_shows_the_dangling_id() {
        assert_eq!(project_label(&entry(None)), "prj_one");
    }

    #[test]
    fn roots_only_is_the_default_and_no_roots_turns_it_off() {
        let default = SessionListArgs {
            all_projects: false,
            project: None,
            archived: false,
            roots: false,
            no_roots: false,
            sort: SessionSortKey::Updated,
            limit: None,
            format: SessionFormat::Table,
        };
        assert!(default.roots_only());
        assert!(
            SessionListArgs {
                roots: true,
                ..default.clone()
            }
            .roots_only()
        );
        assert!(
            !SessionListArgs {
                no_roots: true,
                ..default
            }
            .roots_only()
        );
    }

    #[test]
    fn an_archived_row_keeps_its_marker_at_the_widest_timestamp() {
        let mut listed = entry(None);
        listed.info.time.archived = Some(1);
        // The widest stamp the formatter emits: a two-digit hour, a two-digit
        // month and a two-digit day, i.e. `11:38 PM · 12/31/2026`. Truncating
        // this cell was the defect the column width was sized against, so the
        // assertion is on the marker surviving, not on the cell fitting.
        listed.info.time.updated = 1_798_800_000_000;
        let cell = activity(&listed).expect("format the activity cell");
        assert!(
            cell.ends_with("(archived)"),
            "the archive marker must survive the width cap: {cell}"
        );
        assert!(!cell.contains('…'), "{cell}");
        assert!(utf16_len(&cell) <= ACTIVITY_WIDTH, "{cell}");
    }

    #[test]
    fn a_live_row_carries_no_archive_marker() {
        let cell = activity(&entry(None)).expect("format the activity cell");
        assert!(!cell.contains("archived"), "{cell}");
    }
}

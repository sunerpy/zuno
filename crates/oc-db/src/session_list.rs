//! One listing service for sessions across every project, in the shape the
//! documented endpoint already returns.
//!
//! # Why this exists next to `session::list`
//!
//! [`crate::session::list`] answers "which rows match" and stops there. Two
//! things a cross-project listing needs are not row properties:
//!
//! * the **owning project's** identity, because a list that spans projects is
//!   unreadable when every line says only `prj_9f3c…`;
//! * how many messages a session holds, which lives in another table.
//!
//! Upstream solves the first half in `listGlobal` (`session.ts:557-596`) and
//! nothing solves the second. So this module owns the composed query and the
//! serialised shape, and `session.rs` keeps owning the predicates — see
//! [`crate::session::list_sql`], which this module wraps rather than restates.
//!
//! # The bug this module exists to fix
//!
//! `Session.list()` (`session.ts:548-555`) reads the ambient instance context
//! and pushes `projectID: ctx.project.id` into every listing, and
//! `listByProject` (`session.ts:957-965`) then makes that the first predicate
//! unconditionally. There is no input that turns it off. That is why the CLI's
//! `session list` (`cli/cmd/session.ts:70-88`) can only ever show the checkout
//! you are standing in, even though the data — and the `/experimental/session`
//! endpoint (`server/routes/instance/httpapi/groups/experimental.ts:224-233`) —
//! have always supported more. [`ProjectScope`] is that missing input: the
//! caller says which projects it wants, and nothing is injected behind its back.
//!
//! # `archived` widens, it does not filter
//!
//! `listGlobal` adds `time_archived IS NULL` **unless** `archived` is set
//! (`session.ts:564`), so the flag *adds* archived sessions to a listing that
//! otherwise shows only live ones. It never means "show me only the archived
//! ones". [`GlobalListRequest::archived`] is a `bool` for exactly that reason:
//! the three-way [`crate::session::ArchivedFilter`] would invite a caller to
//! ask for the exclusive variant and quietly change what the flag means.
//!
//! # Ordering
//!
//! Inherited, not restated: `<sort column> DESC, id DESC`, which
//! [`crate::session::list_sql`] emits and the `/api` session surface already
//! serves. Upstream's v2 list sorts on `time_created`
//! (`packages/core/src/session.ts:272`); defaulting to `time_updated` is a
//! declared divergence, and the `id` tie-break is what makes either order total.

use std::collections::BTreeMap;

use oc_error::DbError;
use rusqlite::types::Value;
use rusqlite::{Connection, Row, params, params_from_iter};
use serde::Serialize;
use serde_json::Value as Json;

use crate::open;
use crate::session::{
    ArchivedFilter, ListQuery, ListScope, SessionSort, Summary, Tokens, UPSTREAM_LIST_LIMIT,
    list_sql,
};

/// Which projects a listing covers.
///
/// Two arms and no default, so a caller cannot forget to choose. Upstream's
/// hidden third state — "whatever project the process happens to be in" — is
/// deliberately absent; a caller that wants it resolves the id first and passes
/// [`ProjectScope::Project`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectScope {
    /// Every project. `listGlobal`'s whole-table scan.
    AllProjects,
    /// One project, named by its already-resolved id.
    Project(String),
}

/// A cross-project listing request.
#[derive(Debug, Clone)]
pub struct GlobalListRequest {
    /// Which projects are in range.
    pub scope: ProjectScope,
    /// Only root sessions, i.e. `parent_id IS NULL` (`session.ts:560`).
    pub roots: bool,
    /// Whether archived sessions join the live ones. See the module docs: this
    /// **widens** the result, it does not select archived sessions alone.
    pub archived: bool,
    /// Which timestamp column orders the result.
    pub sort: SessionSort,
    /// Maximum rows. `None` means every match; see [`UPSTREAM_LIST_LIMIT`] for
    /// the number upstream applies and [`GlobalListRequest::effective_limit`]
    /// for how this crate treats it.
    pub limit: Option<u32>,
    /// Substring match on the title, `LIKE`d between `%` wildcards exactly as
    /// upstream does (`session.ts:563`).
    pub search: Option<String>,
}

impl Default for GlobalListRequest {
    fn default() -> Self {
        Self {
            scope: ProjectScope::AllProjects,
            roots: false,
            archived: false,
            sort: SessionSort::Updated,
            limit: None,
            search: None,
        }
    }
}

impl GlobalListRequest {
    /// A listing over every project.
    #[must_use]
    pub fn all_projects() -> Self {
        Self::default()
    }

    /// A listing over one project id.
    #[must_use]
    pub fn project(project_id: impl Into<String>) -> Self {
        Self {
            scope: ProjectScope::Project(project_id.into()),
            ..Self::default()
        }
    }

    /// Include archived sessions alongside the live ones.
    #[must_use]
    pub fn including_archived(mut self) -> Self {
        self.archived = true;
        self
    }

    /// Restrict to root sessions.
    #[must_use]
    pub fn roots_only(mut self) -> Self {
        self.roots = true;
        self
    }

    /// Order by creation time instead of last activity.
    #[must_use]
    pub fn created_order(mut self) -> Self {
        self.sort = SessionSort::Created;
        self
    }

    /// Cap the rows returned.
    #[must_use]
    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }

    /// The row cap this request actually applies.
    ///
    /// [`UPSTREAM_LIST_LIMIT`] is a **default**, not a ceiling. `listGlobal`
    /// reads `input?.limit ?? 100` (`session.ts:575`), so a caller asking for
    /// 500 gets 500 there and gets 500 here; only a caller that asks for
    /// nothing is capped. Clamping to 100 instead would make a 500-row request
    /// silently return a truncated page that is indistinguishable from a
    /// database holding 100 sessions.
    #[must_use]
    pub fn effective_limit(&self) -> u32 {
        self.limit.unwrap_or(UPSTREAM_LIST_LIMIT)
    }

    /// The row-level query this request narrows to.
    fn query(&self) -> ListQuery {
        ListQuery {
            scope: match &self.scope {
                ProjectScope::AllProjects => ListScope::Global,
                ProjectScope::Project(project_id) => ListScope::Project {
                    project_id: project_id.clone(),
                    subpath: None,
                },
            },
            roots: self.roots,
            archived: if self.archived {
                ArchivedFilter::Any
            } else {
                ArchivedFilter::Active
            },
            sort: self.sort,
            limit: Some(self.effective_limit()),
            search: self.search.clone(),
            ..ListQuery::default()
        }
    }
}

/// The three project columns a global listing carries
/// (`ProjectInfo`, `session.ts:247-252`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectInfo {
    /// Project id.
    pub id: String,
    /// Display name, when one was set. Absent rather than null, matching
    /// `name: item.name ?? undefined` (`session.ts:590`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Absolute worktree root.
    pub worktree: String,
}

/// Token usage as the API reports it (`session.ts:98-106`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TokenUsage {
    /// Prompt tokens.
    pub input: i64,
    /// Completion tokens.
    pub output: i64,
    /// Reasoning tokens.
    pub reasoning: i64,
    /// Prompt-cache traffic.
    pub cache: CacheUsage,
}

/// The nested cache counters (`session.ts:102-105`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CacheUsage {
    /// Tokens served from the provider's prompt cache.
    pub read: i64,
    /// Tokens written into it.
    pub write: i64,
}

/// The diff summary, emitted only when a counter was set (`session.ts:60-68`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SummaryInfo {
    /// Lines added.
    pub additions: i64,
    /// Lines removed.
    pub deletions: i64,
    /// Files touched.
    pub files: i64,
    /// Per-file diffs, carried through unparsed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diffs: Option<Json>,
}

/// A share link (`session.ts:69`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShareInfo {
    /// The public URL.
    pub url: String,
}

/// The four session timestamps (`session.ts:111-116`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TimeInfo {
    /// Creation time, Unix milliseconds.
    pub created: i64,
    /// Last-activity time, Unix milliseconds.
    pub updated: i64,
    /// Set while a compaction is in flight.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compacting: Option<i64>,
    /// Set when the session was archived.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived: Option<i64>,
}

/// A session plus its project summary — upstream's `GlobalInfo`
/// (`session.ts:254-258`), which is `Info` spread with a nullable `project`.
///
/// Field names are upstream's, not Rust's: `projectID`, `workspaceID` and
/// `parentID` keep their capitalisation because a client reading
/// `/experimental/session` and a client reading this CLI must not have to know
/// which produced the bytes. Absent optional fields are **omitted**, not null,
/// because that is what `JSON.stringify` does to an `undefined` property; only
/// `project` is explicitly nullable, from `?? null` (`session.ts:595`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GlobalInfo {
    /// `ses_`-prefixed identifier.
    pub id: String,
    /// Short human-facing token.
    pub slug: String,
    /// Owning project id.
    #[serde(rename = "projectID")]
    pub project_id: String,
    /// Owning workspace id.
    #[serde(rename = "workspaceID", skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Absolute directory the session was opened in.
    pub directory: String,
    /// Worktree-relative subpath. `""` at the root, and emitted as `""`:
    /// `row.path ?? undefined` (`session.ts:84`) only drops SQL `NULL`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Parent session id, for a child session.
    #[serde(rename = "parentID", skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Session title.
    pub title: String,
    /// Agent the session last ran under.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Model reference, carried through unparsed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<Json>,
    /// The `opencode` version that created the session.
    pub version: String,
    /// Diff summary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<SummaryInfo>,
    /// Accumulated cost in dollars. Always present: the column is `NOT NULL`
    /// with a zero default, so `cost: row.cost` (`session.ts:97`) never yields
    /// `undefined`.
    #[serde(serialize_with = "serialize_cost")]
    pub cost: f64,
    /// Accumulated token usage, always present for the same reason.
    pub tokens: TokenUsage,
    /// Share link.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share: Option<ShareInfo>,
    /// Caller metadata, carried through unparsed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Json>,
    /// Revert marker, carried through unparsed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revert: Option<Json>,
    /// Permission ruleset, carried through unparsed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission: Option<Json>,
    /// Timestamps.
    pub time: TimeInfo,
    /// The project that owns this session, or `null` when its row is gone.
    pub project: Option<ProjectInfo>,
}

/// One listed session: the wire shape, plus the counts a table needs.
///
/// `messages` is kept **outside** [`GlobalInfo`] on purpose. The endpoint's
/// response has no message count, so folding one in would make this CLI's JSON
/// a superset of `/experimental/session` and turn the differential from an
/// equality into a subset check — losing the ability to notice a missing field.
#[derive(Debug, Clone, PartialEq)]
pub struct ListedSession {
    /// The session as the API reports it.
    pub info: GlobalInfo,
    /// How many `message` rows belong to this session.
    pub messages: i64,
}

/// List sessions across the requested projects, newest activity first.
///
/// One statement. The row filter, its limit and its ordering come from
/// [`crate::session::list_sql`]; this wraps that as a subquery so the limit
/// applies to **sessions** before anything is joined, then attaches the project
/// summary with a `LEFT JOIN` — left, so a session whose project row was
/// deleted still appears with `project: null`, which an inner join would drop
/// and upstream's separate lookup also preserves (`session.ts:595`).
///
/// The message count is a correlated subquery rather than a joined
/// `GROUP BY session_id` aggregate. A grouped aggregate would scan and group
/// the whole `message` table — the largest table in the database — even to list
/// ten sessions, while the correlated form runs one covering-index probe per
/// returned row against `message_session_time_created_id_idx`
/// (`schema.rs:208`), so its cost is bounded by the page size rather than by
/// the message count. Cost needs no aggregate at all: `session.cost` is
/// maintained on the row.
///
/// # Errors
///
/// [`DbError::Query`] if the read fails.
pub fn list(
    connection: &Connection,
    request: &GlobalListRequest,
) -> Result<Vec<ListedSession>, DbError> {
    let (sql, values) = composed_sql(request);
    let mut statement = connection.prepare(&sql).map_err(open::map_error)?;
    let rows = statement
        .query_map(params_from_iter(values), decode)
        .map_err(open::map_error)?;
    let mut listed = Vec::new();
    for row in rows {
        listed.push(row.map_err(open::map_error)?);
    }
    Ok(listed)
}

/// The composed statement and its bindings.
///
/// Named and unit-tested rather than inlined because the `id` tie-break is the
/// one thing here that no fixture can reliably catch. Under a mutation that
/// removes it, SQLite is *free* to return tied rows in any order — and on the
/// data at hand it happened to keep returning them descending, so a behavioural
/// assertion passed while the guarantee was gone. Reading the SQL is the only
/// deterministic detector. Both `ORDER BY` clauses carry it: the inner one so
/// the `LIMIT` picks a stable page, the outer one because it decides what the
/// caller actually sees.
fn composed_sql(request: &GlobalListRequest) -> (String, Vec<Value>) {
    let query = request.query();
    let (inner, values) = list_sql(&query);
    let sort = request.sort.column();
    let sql = format!(
        "SELECT listed.*, project.id, project.name, project.worktree, \
         (SELECT COUNT(*) FROM message WHERE message.session_id = listed.id) \
         FROM ({inner}) AS listed \
         LEFT JOIN project ON project.id = listed.project_id \
         ORDER BY listed.{sort} DESC, listed.id DESC"
    );
    (sql, values)
}

/// Resolve `--project <path|id>` against the `project` table.
///
/// A project id and a worktree path are both unambiguous and neither is
/// guessable from the other — the id is a hash of the Git remote
/// (`oc_paths::project`), so a user standing in a checkout knows the path and a
/// user reading a listing knows the id. Both are accepted, and the id is tried
/// first because it is the primary key.
///
/// Returns `None` when nothing matches, so the caller can say which value was
/// rejected instead of silently listing everything.
///
/// # Errors
///
/// [`DbError::Query`] if the read fails.
pub fn resolve_project(
    connection: &Connection,
    needle: &str,
) -> Result<Option<ProjectInfo>, DbError> {
    let mut statement = connection
        .prepare(
            "SELECT id, name, worktree FROM project WHERE id = ?1 \
             UNION ALL \
             SELECT id, name, worktree FROM project WHERE worktree = ?1 AND id <> ?1 \
             LIMIT 1",
        )
        .map_err(open::map_error)?;
    let mut rows = statement.query(params![needle]).map_err(open::map_error)?;
    let row = rows.next().map_err(open::map_error)?;
    match row {
        Some(row) => Ok(Some(ProjectInfo {
            id: row.get(0).map_err(open::map_error)?,
            name: row.get(1).map_err(open::map_error)?,
            worktree: row.get(2).map_err(open::map_error)?,
        })),
        None => Ok(None),
    }
}

/// Every project that owns at least one session, for a listing's own use.
///
/// # Errors
///
/// [`DbError::Query`] if the read fails.
pub fn projects_with_sessions(connection: &Connection) -> Result<Vec<ProjectInfo>, DbError> {
    let mut statement = connection
        .prepare(
            "SELECT project.id, project.name, project.worktree FROM project \
             WHERE EXISTS (SELECT 1 FROM session WHERE session.project_id = project.id) \
             ORDER BY project.id ASC",
        )
        .map_err(open::map_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok(ProjectInfo {
                id: row.get(0)?,
                name: row.get(1)?,
                worktree: row.get(2)?,
            })
        })
        .map_err(open::map_error)?;
    let mut projects = Vec::new();
    for row in rows {
        projects.push(row.map_err(open::map_error)?);
    }
    Ok(projects)
}

/// Serialise a listing the way `/experimental/session` does.
///
/// # Errors
///
/// [`DbError::Query`] if the shape cannot be encoded, which needs a
/// non-finite `cost` to happen.
pub fn to_json(listed: &[ListedSession]) -> Result<Json, DbError> {
    let infos: Vec<&GlobalInfo> = listed.iter().map(|entry| &entry.info).collect();
    serde_json::to_value(&infos).map_err(|error| DbError::Query {
        source: Box::new(error),
    })
}

/// Decode one composed row: the 29 session columns, then three project columns,
/// then the message count.
fn decode(row: &Row<'_>) -> rusqlite::Result<ListedSession> {
    let session = crate::session::from_row(row)?;
    let project_id: Option<String> = row.get(29)?;
    let project = project_id.map(|id| {
        Ok::<ProjectInfo, rusqlite::Error>(ProjectInfo {
            id,
            name: row.get(30)?,
            worktree: row.get(31)?,
        })
    });
    let project = match project {
        Some(result) => Some(result?),
        None => None,
    };
    let messages: i64 = row.get(32)?;

    Ok(ListedSession {
        info: GlobalInfo {
            id: session.id,
            slug: session.slug,
            project_id: session.project_id,
            workspace_id: session.workspace_id,
            directory: session.directory,
            path: session.path,
            parent_id: session.parent_id,
            title: session.title,
            agent: session.agent,
            model: opaque(session.model.as_deref()),
            version: session.version,
            summary: session.summary.map(summary_info),
            cost: session.cost,
            tokens: token_usage(session.tokens),
            share: session.share_url.map(|url| ShareInfo { url }),
            metadata: opaque(session.metadata.as_deref()),
            revert: opaque(session.revert.as_deref()),
            permission: opaque(session.permission.as_deref()),
            time: TimeInfo {
                created: session.time_created,
                updated: session.time_updated,
                compacting: session.time_compacting,
                archived: session.time_archived,
            },
            project,
        },
        messages,
    })
}

/// Reveal a column the store carries as opaque JSON text.
///
/// Drizzle declares `model`, `metadata`, `revert` and `permission` with
/// `mode: "json"`, so the endpoint hands a client the parsed value, not a
/// string. Text that does not parse is surfaced *as* a string rather than
/// dropped: a session with a corrupt `metadata` blob should still list, and
/// hiding the corruption is how it survives to the next reader.
fn opaque(text: Option<&str>) -> Option<Json> {
    let text = text?;
    Some(serde_json::from_str(text).unwrap_or_else(|_| Json::String(text.to_owned())))
}

/// Write a cost the way `JSON.stringify` writes a JavaScript number.
///
/// JavaScript has one numeric type, so `JSON.stringify({cost: 2})` emits `2`
/// even though the column is SQLite `real`; Rust's `f64` renders the same value
/// as `2.0`. Both parse to the same number, but a client that hashes, caches or
/// diffs the payload sees two different documents — and this crate's promise is
/// that a user can switch between the two binaries. Measured against
/// `/experimental/session` on a shared database: this was the **only** textual
/// difference in the whole listing.
fn serialize_cost<S: serde::Serializer>(cost: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    if cost.fract() == 0.0
        && let Ok(whole) = i64::try_from(*cost as i128)
    {
        return serializer.serialize_i64(whole);
    }
    serializer.serialize_f64(*cost)
}

fn summary_info(summary: Summary) -> SummaryInfo {
    SummaryInfo {
        additions: summary.additions,
        deletions: summary.deletions,
        files: summary.files,
        diffs: opaque(summary.diffs.as_deref()),
    }
}

fn token_usage(tokens: Tokens) -> TokenUsage {
    TokenUsage {
        input: tokens.input,
        output: tokens.output,
        reasoning: tokens.reasoning,
        cache: CacheUsage {
            read: tokens.cache_read,
            write: tokens.cache_write,
        },
    }
}

/// Message counts keyed by session id, for a caller that already has rows.
///
/// # Errors
///
/// [`DbError::Query`] if the read fails.
pub fn message_counts(
    connection: &Connection,
    session_ids: &[String],
) -> Result<BTreeMap<String, i64>, DbError> {
    if session_ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let placeholders = (1..=session_ids.len())
        .map(|slot| format!("?{slot}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT session_id, COUNT(*) FROM message WHERE session_id IN ({placeholders}) \
         GROUP BY session_id"
    );
    let mut statement = connection.prepare(&sql).map_err(open::map_error)?;
    let rows = statement
        .query_map(
            params_from_iter(session_ids.iter().map(|id| Value::Text(id.clone()))),
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .map_err(open::map_error)?;
    let mut counts = BTreeMap::new();
    for row in rows {
        let (id, count) = row.map_err(open::map_error)?;
        counts.insert(id, count);
    }
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_projects_adds_no_project_predicate() {
        let query = GlobalListRequest::all_projects().query();
        assert_eq!(query.scope, ListScope::Global);
    }

    #[test]
    fn one_project_narrows_without_a_subpath() {
        let query = GlobalListRequest::project("prj_a").query();
        assert_eq!(
            query.scope,
            ListScope::Project {
                project_id: String::from("prj_a"),
                subpath: None,
            }
        );
    }

    #[test]
    fn archived_widens_rather_than_filters() {
        assert_eq!(
            GlobalListRequest::all_projects().query().archived,
            ArchivedFilter::Active,
            "a default listing hides archived sessions"
        );
        assert_eq!(
            GlobalListRequest::all_projects()
                .including_archived()
                .query()
                .archived,
            ArchivedFilter::Any,
            "--archived must ADD archived sessions, never select them exclusively"
        );
        assert_ne!(
            GlobalListRequest::all_projects()
                .including_archived()
                .query()
                .archived,
            ArchivedFilter::Archived,
        );
    }

    #[test]
    fn the_upstream_limit_is_a_default_and_not_a_ceiling() {
        assert_eq!(
            GlobalListRequest::all_projects().effective_limit(),
            UPSTREAM_LIST_LIMIT
        );
        assert_eq!(
            GlobalListRequest::all_projects().with_limit(500).limit,
            Some(500)
        );
        assert_eq!(
            GlobalListRequest::all_projects()
                .with_limit(500)
                .effective_limit(),
            500
        );
    }

    #[test]
    fn the_default_sort_is_last_activity() {
        assert_eq!(GlobalListRequest::default().sort, SessionSort::Updated);
        assert_eq!(
            GlobalListRequest::all_projects().created_order().sort,
            SessionSort::Created
        );
    }

    #[test]
    fn every_order_by_carries_the_descending_id_tie_break() {
        for request in [
            GlobalListRequest::all_projects(),
            GlobalListRequest::all_projects().created_order(),
            GlobalListRequest::project("prj_a").roots_only(),
            GlobalListRequest::all_projects()
                .including_archived()
                .with_limit(7),
        ] {
            let (sql, _) = composed_sql(&request);
            let clauses: Vec<&str> = sql
                .match_indices("ORDER BY")
                .map(|(at, _)| &sql[at..])
                .collect();
            assert_eq!(
                clauses.len(),
                2,
                "expected an inner and an outer ORDER BY: {sql}"
            );
            let column = request.sort.column();
            assert!(
                sql.contains(&format!("ORDER BY {column} DESC, id DESC")),
                "the inner page must be ordered with the id tie-break: {sql}"
            );
            assert!(
                sql.contains(&format!("ORDER BY listed.{column} DESC, listed.id DESC")),
                "the outer result must be ordered with the id tie-break: {sql}"
            );
        }
    }

    #[test]
    fn the_message_count_is_correlated_rather_than_grouped() {
        let (sql, _) = composed_sql(&GlobalListRequest::all_projects());
        assert!(
            sql.contains("(SELECT COUNT(*) FROM message WHERE message.session_id = listed.id)"),
            "{sql}"
        );
        assert!(
            !sql.contains("GROUP BY"),
            "a grouped aggregate would scan the whole message table to list one page: {sql}"
        );
        assert!(
            sql.contains("LEFT JOIN project"),
            "an inner join would drop sessions whose project row is gone: {sql}"
        );
        assert!(
            sql.contains("LIMIT"),
            "the limit must apply to the inner session page: {sql}"
        );
    }

    #[test]
    fn an_integral_cost_renders_without_a_fractional_part() {
        #[derive(Serialize)]
        struct Wrapper {
            #[serde(serialize_with = "serialize_cost")]
            cost: f64,
        }
        let rendered = |cost: f64| serde_json::to_string(&Wrapper { cost }).expect("serialise");
        assert_eq!(rendered(2.0), r#"{"cost":2}"#);
        assert_eq!(rendered(0.0), r#"{"cost":0}"#);
        assert_eq!(rendered(-3.0), r#"{"cost":-3}"#);
        assert_eq!(rendered(1.25), r#"{"cost":1.25}"#);
        assert_eq!(rendered(0.125), r#"{"cost":0.125}"#);
        assert_eq!(rendered(f64::NAN), r#"{"cost":null}"#);
        // Integral but outside i64: the cast saturates, `try_from` rejects it,
        // and the f64 path renders it exactly as `JSON.stringify` does.
        assert_eq!(rendered(1e300), r#"{"cost":1e+300}"#);
    }

    #[test]
    fn opaque_columns_survive_being_unparseable() {
        assert_eq!(opaque(None), None);
        assert_eq!(opaque(Some("{\"a\":1}")), Some(serde_json::json!({"a": 1})));
        assert_eq!(
            opaque(Some("not json")),
            Some(Json::String(String::from("not json")))
        );
    }
}

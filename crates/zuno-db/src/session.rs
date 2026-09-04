//! Zuno session rows: create, read, touch, list scopes, and subtree deletion.
//!
//! # Why a delete here is application code and not one `DELETE`
//!
//! Every parent/child link between sessions lives in `session.parent_id`, and
//! that column carries **no foreign key** — the only constraint on the `session`
//! table is `project_id -> project(id) ON DELETE CASCADE` (`schema.rs:153-184`).
//! So `DELETE FROM session WHERE id = ?` removes exactly one row and leaves its
//! children behind, pointing at a parent that no longer exists. They stop
//! appearing under their parent and never appear as roots either, because
//! `parent_id` is not null: they are unreachable rows that still count against
//! every listing, quota and prune scan. Upstream avoids this by walking the tree
//! in application code — `session.ts:619-622` fetches `children(sessionID)` and
//! recurses into `remove` before deleting the parent — and [`remove`] does the
//! same, iteratively, inside one transaction.
//!
//! Two more tables need the same treatment, for two different reasons:
//!
//! * **`part`.** `part.session_id` is indexed (`part_session_idx`) but is *not*
//!   a foreign key; the only FK on `part` is `message_id -> message(id)`. Parts
//!   reached through a message are cascaded away when the session's messages
//!   go, but a part row whose message is already gone — or was never written —
//!   is invisible to that cascade and survives with a dangling `session_id`.
//!   [`remove`] sweeps `part` by `session_id` explicitly.
//! * **`event` and `event_sequence`.** These are keyed by `aggregate_id`, a
//!   plain text column with no relationship to `session.id` that SQLite can
//!   see. Upstream deletes them by hand in `event.ts:513-523`, both statements
//!   in one transaction, and [`remove`] reproduces that step per removed id.
//!
//! # The order of operations
//!
//! Taken from `session.ts:608-629`, with the delete site it defers to at
//! `session/projector.ts:259-261` and the event cleanup at `event.ts:513-523`:
//!
//! 1. read the session, failing if it does not exist (`:609`);
//! 2. cancel that session's background jobs (`:618`, `:940-955`) — not this
//!    crate's business, so [`remove`] returns the removed ids for the caller
//!    that owns the job registry;
//! 3. for each child, recurse (`:619-622`) — children are fully removed before
//!    the parent;
//! 4. delete the `session` row, letting the declared cascades fire
//!    (`projector.ts:259-261`);
//! 5. delete `event_sequence` then `event` for that id (`event.ts:513-523`).
//!
//! # Scope
//!
//! The `session` table only. Message and part persistence, retention and
//! pruning live elsewhere. Columns the schema declares as JSON — `model`,
//! `metadata`, `revert`, `permission`, `summary_diffs` — are carried through
//! verbatim as strings rather than parsed, because nothing here needs to look
//! inside them and re-encoding would lose unknown fields or number formatting.

pub mod path;

use crate::open;
use crate::pool::Pool;
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params, params_from_iter};
use serde_json::{Map, Value as JsonValue};
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};
use zuno_error::DbError;

pub use crate::session::path::session_path;

/// The table this module owns, as it appears in [`DbError::NotFound`].
pub const TABLE: &str = "session";

/// Default title prefix for a session with no parent (`session.ts:48`).
pub const PARENT_TITLE_PREFIX: &str = "New session - ";

/// Default title prefix for a child session (`session.ts:49`).
pub const CHILD_TITLE_PREFIX: &str = "Child session - ";

/// Rows a listing returns when the caller asks for no limit of its own.
///
/// Upstream applies this in two places — `listGlobal`'s `input?.limit ?? 100`
/// (`session.ts:575`) and `listByProject`'s `input.limit ?? 100`
/// (`session.ts:997`) — while the v2 `list` this module's scopes come from
/// applies none at all (`core/src/session.ts:299`). [`ListQuery::limit`] is
/// therefore an explicit `Option`, left unset by [`ListQuery::default`]: a store
/// that silently truncated at 100 would look identical to a store that only had
/// 100 rows. The constant is exported so the request layer can apply upstream's
/// default where upstream applies it.
pub const UPSTREAM_LIST_LIMIT: u32 = 100;

const COLUMNS: &str = "id, project_id, workspace_id, parent_id, slug, directory, path, title, \
     version, share_url, summary_additions, summary_deletions, summary_files, summary_diffs, \
     metadata, cost, tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, \
     tokens_cache_write, tokens_last_prompt, tokens_context_limit, tokens_accounting, \
     tokens_known, tokens_estimated_pending_prompt, tokens_last_confirmed_at, failed_turns, \
     last_failed_at, revert, permission, agent, model, time_created, time_updated, \
     time_compacting, time_archived";
pub(crate) const COLUMN_COUNT: usize = 37;

/// One row of the `session` table.
///
/// Field order follows the table (`schema.rs:153-184`) so a reader can check the
/// two against each other. `tokens` and `summary` are grouped the way
/// `fromRow` (`session.ts:58-121`) groups them, because that is the shape the
/// API emits.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    /// `ses_`-prefixed identifier, descending so newer sorts first.
    pub id: String,
    /// Owning project. The one foreign key on this table, cascading.
    pub project_id: String,
    /// Owning workspace, when the experimental workspace flag is in play.
    pub workspace_id: Option<String>,
    /// Parent session. **No foreign key** — see the module docs.
    pub parent_id: Option<String>,
    /// Short human-facing token, also used to name the session's plan file.
    pub slug: String,
    /// Absolute directory the session was opened in.
    pub directory: String,
    /// `directory` relative to the project worktree; `""` at the root.
    ///
    /// The project-scope subpath filter matches on this. See
    /// [`path::session_path`].
    pub path: Option<String>,
    /// Session title, defaulting to a prefixed timestamp upstream.
    pub title: String,
    /// The Zuno version that created the session.
    pub version: String,
    /// Share link, when the session has been shared.
    pub share_url: Option<String>,
    /// Diff summary, present when any of its three counters is set.
    pub summary: Option<Summary>,
    /// Opaque JSON blob of caller metadata, carried through unparsed.
    pub metadata: Option<String>,
    /// Durable provider usage projection.
    pub usage: SessionUsage,
    /// Opaque JSON revert marker, carried through unparsed.
    pub revert: Option<String>,
    /// Opaque JSON permission ruleset, carried through unparsed.
    pub permission: Option<String>,
    /// Agent the session last ran under.
    pub agent: Option<String>,
    /// Opaque JSON model reference, carried through unparsed.
    pub model: Option<String>,
    /// Creation time, Unix milliseconds.
    pub time_created: i64,
    /// Last-activity time, Unix milliseconds. The default sort column.
    pub time_updated: i64,
    /// Set while a compaction is in flight.
    pub time_compacting: Option<i64>,
    /// Set when the session was archived.
    pub time_archived: Option<i64>,
}

impl Session {
    /// Whether this session is a root, i.e. has no parent.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.parent_id.is_none()
    }

    /// Whether this session has been archived.
    #[must_use]
    pub fn is_archived(&self) -> bool {
        self.time_archived.is_some()
    }

    /// The subpath as the API reports it: `None` for a session at the worktree
    /// root, where the column holds `""`.
    ///
    /// Mirrors `core/src/session/info.ts:42`, which maps the column through
    /// `row.path ? ... : undefined` — an empty string is *absent*, not a path.
    #[must_use]
    pub fn subpath(&self) -> Option<&str> {
        self.path.as_deref().filter(|value| !value.is_empty())
    }
}

/// Accumulated token usage for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tokens {
    /// Prompt tokens.
    pub input: i64,
    /// Completion tokens.
    pub output: i64,
    /// Reasoning tokens.
    pub reasoning: i64,
    /// Tokens served from the provider's prompt cache.
    pub cache_read: i64,
    /// Tokens written into the provider's prompt cache.
    pub cache_write: i64,
}

/// How a provider's prompt figure relates to its cache counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenAccounting {
    /// The provider's input count already includes cache reads and writes.
    CacheInsideInput,
    /// Cache reads and writes sit beside the provider's input count.
    CacheBesideInput,
}

impl TokenAccounting {
    /// Stable persisted spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CacheInsideInput => "cache-inside-input",
            Self::CacheBesideInput => "cache-beside-input",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "cache-inside-input" => Some(Self::CacheInsideInput),
            "cache-beside-input" => Some(Self::CacheBesideInput),
            _ => None,
        }
    }

    fn prompt_total(self, tokens: Tokens) -> i64 {
        match self {
            Self::CacheInsideInput => tokens.input,
            Self::CacheBesideInput => tokens
                .input
                .saturating_add(tokens.cache_read)
                .saturating_add(tokens.cache_write),
        }
    }

    fn normalized(self, tokens: Tokens) -> Tokens {
        let input = match self {
            Self::CacheInsideInput => tokens
                .input
                .saturating_sub(tokens.cache_read)
                .saturating_sub(tokens.cache_write)
                .max(0),
            Self::CacheBesideInput => tokens.input,
        };
        Tokens { input, ..tokens }
    }
}

/// Frontend-neutral durable usage for one session.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SessionUsage {
    /// Accumulated provider cost in dollars.
    pub cost: f64,
    /// Disjoint token buckets. Cache tokens are counted outside `input`.
    pub tokens: Tokens,
    /// Whole prompt sent by the most recent provider request.
    pub last_prompt_tokens: Option<i64>,
    /// Deterministic local estimate for the latest request without confirmed usage.
    pub estimated_pending_prompt_tokens: Option<i64>,
    /// Context ceiling used for the most recent request.
    pub context_limit: Option<i64>,
    /// Accounting mode reported by the most recent request.
    pub accounting: Option<TokenAccounting>,
    /// Whether every assistant token snapshot in this session can be normalized.
    pub known: bool,
    /// Last checkpoint that supplied provider-confirmed usage.
    pub last_confirmed_at: Option<i64>,
    /// Number of turns that ended in an error rather than completion or user cancellation.
    pub failed_turns: i64,
    /// Most recent failed turn timestamp.
    pub last_failed_at: Option<i64>,
}

impl SessionUsage {
    /// Frontend-neutral snapshot. Unknown values remain absent rather than becoming zero.
    #[must_use]
    pub fn snapshot(self) -> zuno_types::UsageSnapshot {
        zuno_types::UsageSnapshot {
            confirmed: zuno_types::TokenUsage {
                input: u64::try_from(self.tokens.input).unwrap_or_default(),
                output: u64::try_from(self.tokens.output).unwrap_or_default(),
                reasoning: u64::try_from(self.tokens.reasoning).unwrap_or_default(),
                cache_read: u64::try_from(self.tokens.cache_read).unwrap_or_default(),
                cache_write: u64::try_from(self.tokens.cache_write).unwrap_or_default(),
                unclassified: 0,
            },
            last_prompt_tokens: self
                .last_prompt_tokens
                .and_then(|value| value.try_into().ok()),
            estimated_pending_prompt_tokens: self
                .estimated_pending_prompt_tokens
                .and_then(|value| value.try_into().ok()),
            context_limit: self.context_limit.and_then(|value| value.try_into().ok()),
            accounting: self.accounting.map_or(
                zuno_types::UsageAccounting::Unknown,
                |accounting| match accounting {
                    TokenAccounting::CacheInsideInput => {
                        zuno_types::UsageAccounting::CacheInsideInput
                    }
                    TokenAccounting::CacheBesideInput => {
                        zuno_types::UsageAccounting::CacheBesideInput
                    }
                },
            ),
            confirmed_known: self.known,
            last_confirmed_at: self.last_confirmed_at,
            failed_turns: u64::try_from(self.failed_turns).unwrap_or_default(),
            last_failed_at: self.last_failed_at,
        }
    }
}

/// One assistant message's usage snapshot, before it is folded into a session.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MessageUsage {
    /// Whether the provider emitted a usage snapshot for this message.
    pub reported: bool,
    /// Cost stored on the assistant message.
    pub cost: f64,
    /// Provider-reported token buckets.
    pub tokens: Tokens,
    /// Provider accounting mode, absent when no reliable usage event arrived.
    pub accounting: Option<TokenAccounting>,
}

impl MessageUsage {
    /// Decode the assistant message JSON written by the engine.
    #[must_use]
    pub fn from_data(data: &Map<String, JsonValue>) -> Self {
        let tokens = data.get("tokens").and_then(JsonValue::as_object);
        let cache = tokens
            .and_then(|tokens| tokens.get("cache"))
            .and_then(JsonValue::as_object);
        Self {
            reported: tokens.is_some(),
            cost: data.get("cost").and_then(JsonValue::as_f64).unwrap_or(0.0),
            tokens: Tokens {
                input: json_i64(tokens.and_then(|tokens| tokens.get("input"))),
                output: json_i64(tokens.and_then(|tokens| tokens.get("output"))),
                reasoning: json_i64(tokens.and_then(|tokens| tokens.get("reasoning"))),
                cache_read: json_i64(cache.and_then(|cache| cache.get("read"))),
                cache_write: json_i64(cache.and_then(|cache| cache.get("write"))),
            },
            accounting: tokens
                .and_then(|tokens| tokens.get("accounting"))
                .and_then(JsonValue::as_str)
                .and_then(TokenAccounting::parse),
        }
    }

    fn normalized(self) -> Option<Tokens> {
        self.accounting
            .map(|accounting| accounting.normalized(self.tokens))
    }

    fn last_prompt_tokens(self) -> Option<i64> {
        self.accounting
            .map(|accounting| accounting.prompt_total(self.tokens))
    }
}

fn json_i64(value: Option<&JsonValue>) -> i64 {
    value
        .and_then(JsonValue::as_i64)
        .or_else(|| {
            value
                .and_then(JsonValue::as_u64)
                .and_then(|value| i64::try_from(value).ok())
        })
        .unwrap_or(0)
        .max(0)
}

/// The diff summary attached to a compacted or reverted session.
///
/// Upstream builds this when *any* of the three counters is non-null and
/// defaults the others to zero (`session.ts:59-67`), so a row with only
/// `summary_files` set still produces a summary.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Summary {
    /// Lines added.
    pub additions: i64,
    /// Lines removed.
    pub deletions: i64,
    /// Files touched.
    pub files: i64,
    /// Opaque JSON array of per-file diffs, carried through unparsed.
    pub diffs: Option<String>,
}

/// The `session.model` column's payload, in the one shape upstream decodes.
///
/// # The key the `session` table uses is not the key the `message` table uses
///
/// A session's model is
/// `Schema.Struct({ id: ModelV2.ID, providerID: ProviderV2.ID, variant: optional(Schema.String) })`
/// (`packages/opencode/src/session/session.ts:220-224`), read back as `row.model.id`
/// (`:88-93`). A *message's* model names the same thing `modelID`
/// (`packages/opencode/src/session/message.ts:121-125`). A session row written with
/// `modelID` therefore has no `id`, and the released binary rejects the entire
/// listing with `Expected string, got undefined` and exit 1 — a Rust turn leaving a
/// database the binary a user rolls back to cannot read.
///
/// Measured against a 5,961-row TypeScript-written `session` table: `id` present in
/// 5,961 rows, `modelID` in 0.
///
/// `variant` is `optional` in that schema and absent from 197 of those rows, so it
/// is omitted rather than emitted as `null` — upstream's writer passes `info.model`
/// through unchanged (`session.ts:130`) and its decoder distinguishes a missing key
/// from an explicit `null`. Nothing in this port has a variant to record at
/// session-creation time.
///
/// Stated once here, beside the column, because a `json!` at each call site is how
/// the two spellings diverged in the first place.
#[must_use]
pub fn model_reference(provider_id: &str, model_id: &str) -> String {
    model_reference_with_variant(provider_id, model_id, None)
}

/// [`model_reference`] carrying the reasoning variant the session last ran with.
///
/// `variant` is the optional third key of the same upstream struct, so a resumed
/// session can restore its reasoning level from the row it already has rather than
/// from a column that would need a format migration. `None` omits the key and yields
/// output byte-identical to [`model_reference`]; it never writes `null`.
#[must_use]
pub fn model_reference_with_variant(
    provider_id: &str,
    model_id: &str,
    variant: Option<&str>,
) -> String {
    #[derive(serde::Serialize)]
    struct ModelReference<'a> {
        id: &'a str,
        #[serde(rename = "providerID")]
        provider_id: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        variant: Option<&'a str>,
    }
    serde_json::to_string(&ModelReference {
        id: model_id,
        provider_id,
        variant,
    })
    .expect("three string fields always serialize")
}

/// A `session.model` column decoded at the durable-storage boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedModel {
    /// Catalog provider id.
    pub provider_id: String,
    /// Catalog model id within the provider.
    pub model_id: String,
    /// Reasoning variant the session last ran with, when one was recorded.
    pub variant: Option<String>,
}

impl PersistedModel {
    /// The `provider/model` spelling turn resolution accepts.
    #[must_use]
    pub fn qualified(&self) -> String {
        format!("{}/{}", self.provider_id, self.model_id)
    }
}

/// Decode a `session.model` column tolerantly.
///
/// Two shapes have been written to this column by supported formats: the JSON
/// object [`model_reference_with_variant`] emits (`variant` optional or `null`), and
/// the plain `provider/model` string an earlier writer stored (see the format-7
/// fixture's `ses_fixture_0001`). Both decode. Anything else — an empty string, a
/// JSON object with a missing or non-string key, an unqualified name — yields
/// `None`, so a resume degrades to configured defaults instead of failing on a row
/// the user cannot repair.
#[must_use]
pub fn decode_model_reference(raw: &str) -> Option<PersistedModel> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<JsonValue>(raw) {
        let object = value.as_object()?;
        let provider_id = object.get("providerID")?.as_str()?;
        let model_id = object.get("id")?.as_str()?;
        let variant = match object.get("variant") {
            None | Some(JsonValue::Null) => None,
            Some(JsonValue::String(variant)) if !variant.is_empty() => Some(variant.clone()),
            Some(_) => None,
        };
        return (!provider_id.is_empty() && !model_id.is_empty()).then(|| PersistedModel {
            provider_id: provider_id.to_owned(),
            model_id: model_id.to_owned(),
            variant,
        });
    }
    let (provider_id, model_id) = raw.split_once('/')?;
    (!provider_id.is_empty() && !model_id.is_empty()).then(|| PersistedModel {
        provider_id: provider_id.to_owned(),
        model_id: model_id.to_owned(),
        variant: None,
    })
}

/// Everything [`create`] needs to write a row.
///
/// `id` and `slug` are supplied by the caller rather than generated here.
/// Upstream generates them outside the store too — `SessionID.descending()` and
/// `Slug.create()` at `session.ts:515-516` — and both are identifier concerns
/// with their own byte format, not storage concerns.
#[derive(Debug, Clone)]
pub struct SessionCreate {
    /// `ses_`-prefixed identifier.
    pub id: String,
    /// Short human-facing token.
    pub slug: String,
    /// Owning project; must already exist, or the foreign key rejects the row.
    pub project_id: String,
    /// The project worktree, used only to derive [`Session::path`].
    pub worktree: String,
    /// Absolute directory the session is being opened in.
    pub directory: String,
    /// Session title.
    pub title: String,
    /// The `opencode` version writing the row.
    pub version: String,
    /// Parent session, for a child session.
    pub parent_id: Option<String>,
    /// Owning workspace.
    pub workspace_id: Option<String>,
    /// Agent the session starts under.
    pub agent: Option<String>,
    /// Opaque JSON model reference.
    pub model: Option<String>,
    /// Opaque JSON caller metadata.
    pub metadata: Option<String>,
    /// Opaque JSON permission ruleset.
    pub permission: Option<String>,
    /// Creation and last-activity time, Unix milliseconds. Both default to now.
    pub time: Option<i64>,
}

impl SessionCreate {
    /// A create input with only the required fields set.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        slug: impl Into<String>,
        project_id: impl Into<String>,
        worktree: impl Into<String>,
        directory: impl Into<String>,
        title: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            slug: slug.into(),
            project_id: project_id.into(),
            worktree: worktree.into(),
            directory: directory.into(),
            title: title.into(),
            version: version.into(),
            parent_id: None,
            workspace_id: None,
            agent: None,
            model: None,
            metadata: None,
            permission: None,
            time: None,
        }
    }

    /// Attach a parent, making this a child session.
    #[must_use]
    pub fn with_parent(mut self, parent_id: impl Into<String>) -> Self {
        self.parent_id = Some(parent_id.into());
        self
    }

    /// Attach a workspace.
    #[must_use]
    pub fn with_workspace(mut self, workspace_id: impl Into<String>) -> Self {
        self.workspace_id = Some(workspace_id.into());
        self
    }

    /// Pin both timestamps, instead of reading the clock.
    #[must_use]
    pub fn at(mut self, millis: i64) -> Self {
        self.time = Some(millis);
        self
    }

    /// The title upstream would default to for this input.
    ///
    /// `session.ts:523` builds `(parentID ? "Child session - " : "New session -
    /// ") + new Date().toISOString()`. The timestamp half is the caller's to
    /// format, because it is the only part that needs a calendar.
    #[must_use]
    pub fn default_title_prefix(parent_id: Option<&str>) -> &'static str {
        if parent_id.is_some() {
            CHILD_TITLE_PREFIX
        } else {
            PARENT_TITLE_PREFIX
        }
    }
}

/// What [`create`] found when it tried to insert.
///
/// Upstream's projector inserts with `onConflictDoNothing` and treats a
/// conflict as `SessionAlreadyProjected` (`projector.ts:215-224`), which the v2
/// create path catches and resolves in favour of the row already on disk:
/// "Concurrent creation lost the projection race. The existing Session identity
/// wins." (`core/src/session.ts:249-259`). Reporting which happened, rather
/// than returning a bare row, is what lets a caller tell a fresh session from
/// one it just lost a race for.
#[derive(Debug, Clone, PartialEq)]
pub enum Creation {
    /// The row was written by this call.
    Inserted(Session),
    /// A row with this id already existed and won.
    AlreadyExists(Session),
}

impl Creation {
    /// The session either way.
    #[must_use]
    pub fn session(&self) -> &Session {
        match self {
            Self::Inserted(session) | Self::AlreadyExists(session) => session,
        }
    }

    /// Take the session, discarding which branch produced it.
    #[must_use]
    pub fn into_session(self) -> Session {
        match self {
            Self::Inserted(session) | Self::AlreadyExists(session) => session,
        }
    }

    /// Whether this call is the one that wrote the row.
    #[must_use]
    pub fn was_inserted(&self) -> bool {
        matches!(self, Self::Inserted(_))
    }
}

/// Which sessions a listing is scoped to.
///
/// The three arms are mutually exclusive because upstream's schema makes them
/// so: `ListInput` is `Schema.Union([ListDirectoryInput, ListProjectInput,
/// ListAllInput])` (`core/src/session.ts:76`), where `directory` is required in
/// the first, `project` is required in the second, and the third has neither.
/// An enum is that union; a struct of three optional fields would let a caller
/// ask for two scopes at once, which the schema cannot express.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ListScope {
    /// Sessions opened in one absolute directory.
    Directory {
        /// The absolute directory to match exactly.
        directory: String,
    },
    /// Sessions belonging to one project, optionally narrowed to a subpath.
    Project {
        /// The project id to match.
        project_id: String,
        /// A worktree-relative path; matches that directory and everything
        /// under it. See [`ListScope::Project::subpath`] handling in [`list`].
        subpath: Option<String>,
    },
    /// Every session, across every project. Upstream's `listGlobal`.
    #[default]
    Global,
}

/// Which column a listing sorts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionSort {
    /// `time_updated`, the default. Matches `listGlobal`
    /// (`session.ts:574`) and `listByProject` (`session.ts:1000`).
    #[default]
    Updated,
    /// `time_created`, the column the v2 `list` sorts on
    /// (`core/src/session.ts:272`).
    Created,
}

impl SessionSort {
    pub(crate) fn column(self) -> &'static str {
        match self {
            Self::Updated => "time_updated",
            Self::Created => "time_created",
        }
    }
}

/// Which way a listing sorts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortDirection {
    /// Newest first. Upstream's default (`core/src/session.ts:270`).
    #[default]
    Descending,
    /// Oldest first.
    Ascending,
}

impl SortDirection {
    fn keyword(self) -> &'static str {
        match self {
            Self::Descending => "DESC",
            Self::Ascending => "ASC",
        }
    }
}

/// Whether a listing includes archived sessions.
///
/// Upstream is inconsistent here: `listGlobal` excludes archived sessions
/// unless asked (`session.ts:564`), while `listByProject` has no archived
/// handling at all and returns them. Rather than pick one and make the
/// behaviour depend on the scope, this is an explicit filter whose default
/// excludes nothing — a listing that silently hid rows would be indistinguishable
/// from a listing that had none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ArchivedFilter {
    /// Archived and active sessions alike. The default.
    #[default]
    Any,
    /// Only sessions that have not been archived. `listGlobal`'s behaviour.
    Active,
    /// Only sessions that have been archived.
    Archived,
}

/// A session listing request.
///
/// Every field beyond [`ListQuery::scope`] narrows the result and defaults to
/// not narrowing it.
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    /// Which sessions are in range at all.
    pub scope: ListScope,
    /// Restrict to one workspace (`session.ts:559`, `core/src/session.ts:275`).
    pub workspace_id: Option<String>,
    /// Substring match on the title. Passed to `LIKE` between `%` wildcards
    /// exactly as upstream does (`session.ts:563`), so `%` and `_` in the term
    /// keep their wildcard meaning.
    pub search: Option<String>,
    /// Only root sessions, i.e. `parent_id IS NULL` (`session.ts:560`).
    pub roots: bool,
    /// Lower bound, inclusive, on the sort column (`session.ts:561`).
    pub start: Option<i64>,
    /// Upper bound, exclusive, on the sort column — upstream's keyset cursor
    /// (`session.ts:562`).
    pub cursor: Option<i64>,
    /// Whether archived sessions are in range.
    pub archived: ArchivedFilter,
    /// Which column to sort on.
    pub sort: SessionSort,
    /// Which way to sort.
    pub direction: SortDirection,
    /// Maximum rows. See [`UPSTREAM_LIST_LIMIT`].
    pub limit: Option<u32>,
}

impl ListQuery {
    /// A listing over every session.
    #[must_use]
    pub fn global() -> Self {
        Self::default()
    }

    /// A listing over one absolute directory.
    #[must_use]
    pub fn directory(directory: impl Into<String>) -> Self {
        Self {
            scope: ListScope::Directory {
                directory: directory.into(),
            },
            ..Self::default()
        }
    }

    /// A listing over one project.
    #[must_use]
    pub fn project(project_id: impl Into<String>) -> Self {
        Self {
            scope: ListScope::Project {
                project_id: project_id.into(),
                subpath: None,
            },
            ..Self::default()
        }
    }

    /// Narrow a project listing to a worktree-relative subpath.
    ///
    /// Ignored — and reported as ignored by
    /// [`ListQuery::subpath_applies`] — on any other scope, because upstream
    /// declares `subpath` only on the project arm of the union
    /// (`core/src/session.ts:64-68`).
    #[must_use]
    pub fn with_subpath(mut self, subpath: impl Into<String>) -> Self {
        if let ListScope::Project {
            subpath: slot,
            project_id: _,
        } = &mut self.scope
        {
            *slot = Some(subpath.into());
        }
        self
    }

    /// Sort on `time_created` instead of `time_updated`.
    #[must_use]
    pub fn created_order(mut self) -> Self {
        self.sort = SessionSort::Created;
        self
    }

    /// Cap the number of rows returned.
    #[must_use]
    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Exclude archived sessions, as `listGlobal` does.
    #[must_use]
    pub fn active_only(mut self) -> Self {
        self.archived = ArchivedFilter::Active;
        self
    }

    /// Whether this query carries a subpath that will actually be applied.
    ///
    /// A non-empty subpath on the project scope filters. Anything else — a
    /// subpath on the directory or global scope, or an empty one — does not, and
    /// upstream's own `listByProject` agrees: its `if (input.path)` guard
    /// (`session.ts:969`) skips an empty string.
    #[must_use]
    pub fn subpath_applies(&self) -> bool {
        matches!(
            &self.scope,
            ListScope::Project {
                subpath: Some(subpath),
                ..
            } if !subpath.is_empty()
        )
    }
}

/// A session paired with a summary of the project that owns it.
///
/// `listGlobal` returns rows that span projects, so each carries enough project
/// identity to be rendered without a second lookup (`GlobalInfo`,
/// `session.ts:254-258`). `project` is `None` when the row's `project_id` has
/// no matching row, which upstream also permits via `?? null`
/// (`session.ts:595`).
#[derive(Debug, Clone, PartialEq)]
pub struct GlobalSession {
    /// The session row.
    pub session: Session,
    /// The project that owns it, when it still exists.
    pub project: Option<ProjectSummary>,
}

/// The three project columns a global listing carries (`session.ts:247-252`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSummary {
    /// Project id.
    pub id: String,
    /// Display name, when one was set.
    pub name: Option<String>,
    /// Absolute worktree root.
    pub worktree: String,
}

/// Write one session row.
///
/// `path` is derived from `worktree` and `directory` by
/// [`path::session_path`]; every other column comes from `input` or the
/// schema's default. An id that already exists leaves the stored row untouched
/// and returns [`Creation::AlreadyExists`], matching
/// `projector.ts:215-224`.
///
/// # Errors
///
/// [`DbError::Query`] if the insert or the follow-up read fails — including
/// when `project_id` names no project, which the table's foreign key rejects.
/// [`DbError::Busy`] if another writer holds the lock.
pub fn create(transaction: &Transaction<'_>, input: &SessionCreate) -> Result<Creation, DbError> {
    let now = match input.time {
        Some(millis) => millis,
        None => unix_milliseconds()?,
    };
    let relative = session_path(input.worktree.as_ref(), input.directory.as_ref());

    let inserted = transaction
        .execute(
            "INSERT INTO session (\
               id, project_id, workspace_id, parent_id, slug, directory, path, title, version, \
               metadata, cost, tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, \
               tokens_cache_write, permission, agent, model, time_created, time_updated\
             ) VALUES (\
               ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0, 0, 0, 0, 0, 0, ?11, ?12, ?13, ?14, ?14\
             ) ON CONFLICT (id) DO NOTHING",
            params![
                input.id,
                input.project_id,
                input.workspace_id,
                input.parent_id,
                input.slug,
                input.directory,
                relative,
                input.title,
                input.version,
                input.metadata,
                input.permission,
                input.agent,
                input.model,
                now,
            ],
        )
        .map_err(open::map_error)?;

    let stored = get(transaction, &input.id)?;
    if inserted == 0 {
        return Ok(Creation::AlreadyExists(stored));
    }
    Ok(Creation::Inserted(stored))
}

/// Read one session by id.
///
/// # Errors
///
/// [`DbError::NotFound`] when no row has that id, mirroring `session.ts:544`.
/// [`DbError::Query`] if the read fails.
pub fn get(connection: &Connection, id: &str) -> Result<Session, DbError> {
    find(connection, id)?.ok_or_else(|| DbError::NotFound {
        table: TABLE.to_owned(),
        id: id.to_owned(),
    })
}

/// Read one session by id, or `None` when it does not exist.
///
/// # Errors
///
/// [`DbError::Query`] if the read fails.
pub fn find(connection: &Connection, id: &str) -> Result<Option<Session>, DbError> {
    let sql = format!("SELECT {COLUMNS} FROM session WHERE id = ?1");
    let mut statement = connection.prepare(&sql).map_err(open::map_error)?;
    let mut rows = statement.query(params![id]).map_err(open::map_error)?;
    let row = rows.next().map_err(open::map_error)?;
    match row {
        Some(row) => Ok(Some(from_row(row).map_err(open::map_error)?)),
        None => Ok(None),
    }
}

/// Set `time_updated` to now, returning the value written.
///
/// `session.ts:751-753` patches nothing but `time.updated`. A session that does
/// not exist updates nothing and reports [`DbError::NotFound`] rather than
/// succeeding silently, because `touch` upstream goes through `patch`, which
/// reads the session first.
///
/// # Errors
///
/// [`DbError::NotFound`] when no row has that id. [`DbError::Query`] if the
/// clock or the update fails.
pub fn touch(transaction: &Transaction<'_>, id: &str) -> Result<i64, DbError> {
    let now = unix_milliseconds()?;
    touch_at(transaction, id, now)
}

/// Set `time_updated` to `millis`, returning it.
///
/// # Errors
///
/// [`DbError::NotFound`] when no row has that id. [`DbError::Query`] if the
/// update fails.
pub fn touch_at(transaction: &Transaction<'_>, id: &str, millis: i64) -> Result<i64, DbError> {
    let updated = transaction
        .execute(
            "UPDATE session SET time_updated = ?2 WHERE id = ?1",
            params![id, millis],
        )
        .map_err(open::map_error)?;
    if updated == 0 {
        return Err(DbError::NotFound {
            table: TABLE.to_owned(),
            id: id.to_owned(),
        });
    }
    Ok(millis)
}

/// Reconcile one assistant message's old and new usage snapshots into its session.
///
/// The message upsert and this call must share a transaction. Replaying the same
/// checkpoint then reads the just-persisted snapshot as `previous`, making every delta
/// zero instead of charging the session twice. When any assistant message lacks an
/// accounting mode, cumulative token usage remains explicitly unknown; the latest prompt
/// may still be reported when the current message itself is known.
///
/// # Errors
///
/// [`DbError::NotFound`] when `session_id` names no session. [`DbError::Query`] when
/// SQLite cannot inspect or update the projection.
pub fn reconcile_usage(
    transaction: &Transaction<'_>,
    session_id: &str,
    previous: Option<MessageUsage>,
    current: MessageUsage,
    context_limit: Option<i64>,
) -> Result<(), DbError> {
    if !current.reported {
        return Ok(());
    }

    let was_known = transaction
        .query_row(
            "SELECT tokens_known FROM session WHERE id = ?1",
            [session_id],
            |row| row.get::<_, bool>(0),
        )
        .optional()
        .map_err(open::map_error)?
        .ok_or_else(|| DbError::NotFound {
            table: TABLE.to_owned(),
            id: session_id.to_owned(),
        })?;
    let all_known = transaction
        .query_row(
            "SELECT
               EXISTS (
                 SELECT 1 FROM message
                 WHERE session_id = ?1
                   AND json_extract(data, '$.role') = 'assistant'
                   AND json_type(data, '$.tokens') = 'object'
               )
               AND NOT EXISTS (
                 SELECT 1 FROM message
                 WHERE session_id = ?1
                   AND json_extract(data, '$.role') = 'assistant'
                   AND json_type(data, '$.tokens') = 'object'
                   AND (
                     json_extract(data, '$.tokens.accounting') IS NULL
                     OR json_extract(data, '$.tokens.accounting')
                        NOT IN ('cache-inside-input', 'cache-beside-input')
                   )
               )",
            [session_id],
            |row| row.get::<_, bool>(0),
        )
        .map_err(open::map_error)?;

    let accounting = current.accounting.map(TokenAccounting::as_str);
    let last_prompt = current.last_prompt_tokens();
    let context_limit = context_limit.filter(|value| *value > 0);
    let confirmed_at = unix_milliseconds()?;

    if all_known && was_known {
        let old = previous
            .and_then(MessageUsage::normalized)
            .unwrap_or_default();
        let new = current.normalized().unwrap_or_default();
        transaction
            .execute(
                "UPDATE session
                 SET cost = max(0.0, cost + ?2 - ?3),
                     tokens_input = max(0, tokens_input + ?4 - ?5),
                     tokens_output = max(0, tokens_output + ?6 - ?7),
                     tokens_reasoning = max(0, tokens_reasoning + ?8 - ?9),
                     tokens_cache_read = max(0, tokens_cache_read + ?10 - ?11),
                     tokens_cache_write = max(0, tokens_cache_write + ?12 - ?13),
                     tokens_last_prompt = ?14,
                     tokens_context_limit = coalesce(?15, tokens_context_limit),
                     tokens_accounting = ?16,
                     tokens_known = 1,
                     tokens_estimated_pending_prompt = NULL,
                     tokens_last_confirmed_at = ?17
                 WHERE id = ?1",
                params![
                    session_id,
                    current.cost,
                    previous.map_or(0.0, |usage| usage.cost),
                    new.input,
                    old.input,
                    new.output,
                    old.output,
                    new.reasoning,
                    old.reasoning,
                    new.cache_read,
                    old.cache_read,
                    new.cache_write,
                    old.cache_write,
                    last_prompt,
                    context_limit,
                    accounting,
                    confirmed_at,
                ],
            )
            .map_err(open::map_error)?;
        return Ok(());
    }

    if all_known {
        transaction
            .execute(
                "UPDATE session
                 SET cost = coalesce((
                       SELECT sum(coalesce(json_extract(message.data, '$.cost'), 0))
                       FROM message
                       WHERE message.session_id = session.id
                         AND json_extract(message.data, '$.role') = 'assistant'
                     ), 0),
                     tokens_input = coalesce((
                       SELECT sum(CASE json_extract(message.data, '$.tokens.accounting')
                         WHEN 'cache-inside-input' THEN max(
                           coalesce(json_extract(message.data, '$.tokens.input'), 0)
                           - coalesce(json_extract(message.data, '$.tokens.cache.read'), 0)
                           - coalesce(json_extract(message.data, '$.tokens.cache.write'), 0),
                           0
                         )
                         WHEN 'cache-beside-input' THEN
                           coalesce(json_extract(message.data, '$.tokens.input'), 0)
                         ELSE 0
                       END)
                       FROM message
                       WHERE message.session_id = session.id
                         AND json_extract(message.data, '$.role') = 'assistant'
                     ), 0),
                     tokens_output = coalesce((
                       SELECT sum(coalesce(json_extract(message.data, '$.tokens.output'), 0))
                       FROM message
                       WHERE message.session_id = session.id
                         AND json_extract(message.data, '$.role') = 'assistant'
                     ), 0),
                     tokens_reasoning = coalesce((
                       SELECT sum(coalesce(json_extract(message.data, '$.tokens.reasoning'), 0))
                       FROM message
                       WHERE message.session_id = session.id
                         AND json_extract(message.data, '$.role') = 'assistant'
                     ), 0),
                     tokens_cache_read = coalesce((
                       SELECT sum(coalesce(json_extract(message.data, '$.tokens.cache.read'), 0))
                       FROM message
                       WHERE message.session_id = session.id
                         AND json_extract(message.data, '$.role') = 'assistant'
                     ), 0),
                     tokens_cache_write = coalesce((
                       SELECT sum(coalesce(json_extract(message.data, '$.tokens.cache.write'), 0))
                       FROM message
                       WHERE message.session_id = session.id
                         AND json_extract(message.data, '$.role') = 'assistant'
                     ), 0),
                     tokens_last_prompt = ?2,
                     tokens_context_limit = coalesce(?3, tokens_context_limit),
                     tokens_accounting = ?4,
                     tokens_known = 1,
                     tokens_estimated_pending_prompt = NULL,
                     tokens_last_confirmed_at = ?5
                 WHERE id = ?1",
                params![
                    session_id,
                    last_prompt,
                    context_limit,
                    accounting,
                    confirmed_at
                ],
            )
            .map_err(open::map_error)?;
        return Ok(());
    }

    transaction
        .execute(
            "UPDATE session
             SET cost = max(0.0, cost + ?2 - ?3),
                 tokens_last_prompt = ?4,
                 tokens_context_limit = coalesce(?5, tokens_context_limit),
                 tokens_accounting = ?6,
                 tokens_known = 0,
                 tokens_estimated_pending_prompt = NULL,
                 tokens_last_confirmed_at = ?7
             WHERE id = ?1",
            params![
                session_id,
                current.cost,
                previous.map_or(0.0, |usage| usage.cost),
                last_prompt,
                context_limit,
                accounting,
                confirmed_at,
            ],
        )
        .map_err(open::map_error)?;
    Ok(())
}

/// Persist the local prompt estimate before an HTTP request is attempted.
///
/// This never changes confirmed counters. A provider rejection, timeout, or cancellation
/// therefore leaves the last trustworthy total intact while the client can still show
/// the request that was attempted as an approximation.
pub fn record_provider_request_started(
    connection: &Connection,
    session_id: &str,
    estimated_prompt_tokens: u64,
    context_limit: Option<u64>,
) -> Result<(), DbError> {
    let estimated = i64::try_from(estimated_prompt_tokens).unwrap_or(i64::MAX);
    let context_limit = context_limit.and_then(|value| i64::try_from(value).ok());
    let updated = connection
        .execute(
            "UPDATE session
             SET tokens_estimated_pending_prompt = ?2,
                 tokens_context_limit = coalesce(?3, tokens_context_limit)
             WHERE id = ?1",
            params![session_id, estimated, context_limit],
        )
        .map_err(open::map_error)?;
    if updated == 0 {
        return Err(DbError::NotFound {
            table: TABLE.to_owned(),
            id: session_id.to_owned(),
        });
    }
    Ok(())
}

/// Record one failed top-level turn without mutating confirmed usage.
pub fn record_turn_failure(connection: &Connection, session_id: &str) -> Result<i64, DbError> {
    let failed_at = unix_milliseconds()?;
    let updated = connection
        .execute(
            "UPDATE session
             SET failed_turns = failed_turns + 1,
                 last_failed_at = ?2
             WHERE id = ?1",
            params![session_id, failed_at],
        )
        .map_err(open::map_error)?;
    if updated == 0 {
        return Err(DbError::NotFound {
            table: TABLE.to_owned(),
            id: session_id.to_owned(),
        });
    }
    Ok(failed_at)
}

/// Replace a session's title, returning the millisecond it was updated at.
///
/// Separate from [`touch_at`] rather than folded into a general `patch` because
/// the title is the only session column a *model* writes: the engine's `title`
/// internal generates one from the first exchange, and upstream's own writer is
/// likewise a one-column update (`session.ts:755-757`). Keeping it one column wide
/// means a title write can never clobber a field the turn is concurrently changing.
///
/// # Errors
///
/// [`DbError::NotFound`] when no row has that id. [`DbError::Query`] if the clock
/// or the update fails.
pub fn set_title(transaction: &Transaction<'_>, id: &str, title: &str) -> Result<i64, DbError> {
    let now = unix_milliseconds()?;
    let updated = transaction
        .execute(
            "UPDATE session SET title = ?2, time_updated = ?3 WHERE id = ?1",
            params![id, title, now],
        )
        .map_err(open::map_error)?;
    if updated == 0 {
        return Err(DbError::NotFound {
            table: TABLE.to_owned(),
            id: id.to_owned(),
        });
    }
    Ok(now)
}

/// Replace a session's opaque caller metadata and advance its activity time.
///
/// Runtime-owned identities use this after session creation once their resolved fields are
/// known. Keeping the mutation one column wide prevents a continuation checkpoint from
/// clobbering the agent, model, usage, or lifecycle fields updated by the turn itself.
///
/// # Errors
///
/// [`DbError::NotFound`] when no row has `id`. [`DbError::Query`] if the clock or update fails.
pub fn set_metadata(
    transaction: &Transaction<'_>,
    id: &str,
    metadata: &str,
) -> Result<i64, DbError> {
    let now = unix_milliseconds()?;
    let updated = transaction
        .execute(
            "UPDATE session SET metadata = ?2, time_updated = ?3 WHERE id = ?1",
            params![id, metadata, now],
        )
        .map_err(open::map_error)?;
    if updated == 0 {
        return Err(DbError::NotFound {
            table: TABLE.to_owned(),
            id: id.to_owned(),
        });
    }
    Ok(now)
}

/// Replace the agent and append the corresponding projected switch message.
pub fn switch_agent_at(
    transaction: &Transaction<'_>,
    id: &str,
    message_id: &str,
    agent: &str,
    millis: i64,
) -> Result<(), DbError> {
    update_session_column(transaction, id, "agent", agent, millis)?;
    append_switch_message(
        transaction,
        id,
        message_id,
        "agent-switched",
        &serde_json::json!({
            "agent": agent,
            "time": {"created": millis},
        }),
        millis,
    )
}

/// Replace the serialized model reference and append its projected switch message.
pub fn switch_model_at(
    transaction: &Transaction<'_>,
    id: &str,
    message_id: &str,
    model: &str,
    millis: i64,
) -> Result<(), DbError> {
    let model_value =
        serde_json::from_str::<serde_json::Value>(model).map_err(|source| DbError::Decode {
            table: TABLE.to_owned(),
            source,
        })?;
    update_session_column(transaction, id, "model", model, millis)?;
    append_switch_message(
        transaction,
        id,
        message_id,
        "model-switched",
        &serde_json::json!({
            "model": model_value,
            "time": {"created": millis},
        }),
        millis,
    )
}

fn append_switch_message(
    transaction: &Transaction<'_>,
    session_id: &str,
    message_id: &str,
    kind: &str,
    data: &serde_json::Value,
    millis: i64,
) -> Result<(), DbError> {
    let data = serde_json::to_string(data).expect("JSON values always serialize");
    transaction
        .execute(
            "INSERT INTO session_message \
             (id, session_id, type, seq, time_created, time_updated, data) \
             VALUES (?1, ?2, ?3, \
               (SELECT COALESCE(MAX(seq), -1) + 1 FROM session_message WHERE session_id = ?2), \
               ?4, ?4, ?5)",
            params![message_id, session_id, kind, millis, data],
        )
        .map_err(open::map_error)?;
    Ok(())
}

/// Stage a reversible transcript boundary after proving the boundary exists.
pub fn stage_revert_at(
    transaction: &Transaction<'_>,
    id: &str,
    message_id: &str,
    revert: &str,
    millis: i64,
) -> Result<(), DbError> {
    get(transaction, id)?;
    let exists = transaction
        .query_row(
            "SELECT 1 FROM session_message WHERE session_id = ?1 AND id = ?2 \
             UNION ALL SELECT 1 FROM message WHERE session_id = ?1 AND id = ?2 LIMIT 1",
            params![id, message_id],
            |_| Ok(()),
        )
        .optional()
        .map_err(open::map_error)?
        .is_some();
    if !exists {
        return Err(DbError::NotFound {
            table: "message".to_owned(),
            id: message_id.to_owned(),
        });
    }
    update_session_column(transaction, id, "revert", revert, millis)
}

/// Clear a staged boundary without deleting transcript rows.
pub fn clear_revert_at(
    transaction: &Transaction<'_>,
    id: &str,
    millis: i64,
) -> Result<(), DbError> {
    if get(transaction, id)?.revert.is_none() {
        return Ok(());
    }
    let updated = transaction
        .execute(
            "UPDATE session SET revert = NULL, time_updated = ?2 WHERE id = ?1",
            params![id, millis],
        )
        .map_err(open::map_error)?;
    require_updated(updated, id)
}

/// Permanently discard transcript rows after the staged boundary.
///
/// The boundary is the staged message's `(time_created, id)` pair, read from the
/// projected transcript when the message was projected and from the legacy
/// `message` table otherwise. The same pair bounds the deletion in both tables, so
/// a boundary that exists in only one of them still trims the other.
///
/// Inbox rows are never deleted. Every `queued`, `steering`, or `promoted` input
/// was aimed at the discarded tail, so each is retired through the ordinary
/// cancellation transition and logs its own `session.input.cancelled`; consumed
/// rows are immutable history and stay untouched. The commit ends with one
/// `session.reverted` event carrying the boundary, the removed-row counts, and the
/// retired input identifiers, so the discarded tail is reconstructable from the
/// durable log.
///
/// Returns `false` when the session exists but has no staged boundary. Callers use
/// that result as the destructive-operation confirmation guard.
pub fn commit_revert_at(
    transaction: &Transaction<'_>,
    id: &str,
    millis: i64,
) -> Result<bool, DbError> {
    let session = get(transaction, id)?;
    let Some(raw) = session.revert else {
        return Ok(false);
    };
    let marker: serde_json::Value =
        serde_json::from_str(&raw).map_err(|source| DbError::Decode {
            table: TABLE.to_owned(),
            source,
        })?;
    let message_id = marker
        .get("messageID")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| DbError::Decode {
            table: TABLE.to_owned(),
            source: serde_json::from_str::<serde_json::Value>("{").expect_err("invalid JSON"),
        })?
        .to_owned();

    let boundary = match revert_boundary(transaction, "session_message", id, &message_id)? {
        Some(boundary) => boundary,
        None => revert_boundary(transaction, "message", id, &message_id)?.ok_or_else(|| {
            DbError::NotFound {
                table: "message".to_owned(),
                id: message_id.clone(),
            }
        })?,
    };
    let (boundary_created, boundary_id) = boundary;

    let mut cancelled_input_ids = Vec::new();
    for input in crate::inbox::unconsumed_in(transaction, id)? {
        let retired = crate::inbox::transition_in(
            transaction,
            id,
            &input.id,
            &[
                crate::inbox::SubmissionState::Queued,
                crate::inbox::SubmissionState::Steering,
                crate::inbox::SubmissionState::Promoted,
            ],
            crate::inbox::SubmissionState::Cancelled,
            None,
            "session.input.cancelled",
        )?;
        if retired.is_some() {
            cancelled_input_ids.push(serde_json::Value::String(input.id));
        }
    }

    let removed_messages = transaction
        .execute(
            "DELETE FROM session_message WHERE session_id = ?1 \
             AND (time_created > ?2 OR (time_created = ?2 AND id > ?3))",
            params![id, boundary_created, boundary_id],
        )
        .map_err(open::map_error)?;
    let removed_legacy_messages = transaction
        .execute(
            "DELETE FROM message WHERE session_id = ?1 \
             AND (time_created > ?2 OR (time_created = ?2 AND id > ?3))",
            params![id, boundary_created, boundary_id],
        )
        .map_err(open::map_error)?;
    let cleared_epochs = transaction
        .execute(
            "DELETE FROM session_context_epoch WHERE session_id = ?1",
            params![id],
        )
        .map_err(open::map_error)?;
    clear_revert_at(transaction, id, millis)?;

    let properties: serde_json::Map<String, serde_json::Value> = [
        (
            "sessionID".to_owned(),
            serde_json::Value::String(id.to_owned()),
        ),
        (
            "messageID".to_owned(),
            serde_json::Value::String(message_id),
        ),
        ("marker".to_owned(), marker),
        (
            "boundaryTimeCreated".to_owned(),
            serde_json::Value::Number(boundary_created.into()),
        ),
        (
            "removedMessageCount".to_owned(),
            serde_json::Value::Number(removed_messages.into()),
        ),
        (
            "removedLegacyMessageCount".to_owned(),
            serde_json::Value::Number(removed_legacy_messages.into()),
        ),
        (
            "cancelledInputIDs".to_owned(),
            serde_json::Value::Array(cancelled_input_ids),
        ),
        (
            "contextEpochCleared".to_owned(),
            serde_json::Value::Bool(cleared_epochs > 0),
        ),
        (
            "timeUpdated".to_owned(),
            serde_json::Value::Number(millis.into()),
        ),
    ]
    .into_iter()
    .collect();
    crate::event_log::append_in(
        transaction,
        id,
        crate::event_log::NewSessionEvent::new("session.reverted", properties)?,
    )?;
    Ok(true)
}

/// The `(time_created, id)` position of one message in `table`, if it is there.
fn revert_boundary(
    transaction: &Transaction<'_>,
    table: &'static str,
    session_id: &str,
    message_id: &str,
) -> Result<Option<(i64, String)>, DbError> {
    debug_assert!(matches!(table, "session_message" | "message"));
    transaction
        .query_row(
            &format!("SELECT time_created, id FROM {table} WHERE session_id = ?1 AND id = ?2"),
            params![session_id, message_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(open::map_error)
}

fn update_session_column(
    transaction: &Transaction<'_>,
    id: &str,
    column: &'static str,
    value: &str,
    millis: i64,
) -> Result<(), DbError> {
    debug_assert!(matches!(column, "agent" | "model" | "revert"));
    let updated = transaction
        .execute(
            &format!("UPDATE session SET {column} = ?2, time_updated = ?3 WHERE id = ?1"),
            params![id, value, millis],
        )
        .map_err(open::map_error)?;
    require_updated(updated, id)
}

fn require_updated(updated: usize, id: &str) -> Result<(), DbError> {
    if updated == 0 {
        return Err(DbError::NotFound {
            table: TABLE.to_owned(),
            id: id.to_owned(),
        });
    }
    Ok(())
}

/// Whether `title` is still the one [`create`] invented, and so may be replaced.
///
/// Lives next to [`SessionCreate::default_title_prefix`] for the same reason
/// [`crate::message::created_after`] lives next to the `ORDER BY` it escapes: the
/// predicate is only correct for exactly the strings that function produces, and a
/// reader who changes one has to see the other.
///
/// Upstream tests the two prefixes followed by an ISO-8601 instant
/// (`session.ts:51-55`). This accepts the prefix with *any* suffix, and also the
/// bare prefix with its trailing separator trimmed, because this port's own
/// [`create`] callers have written `"New session"` with no timestamp. Being laxer
/// only ever means a generated title replaces a placeholder; being stricter would
/// mean a session the user never named stays unnamed forever, which is the failure
/// the `title` internal exists to prevent.
#[must_use]
pub fn is_default_title(title: &str) -> bool {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return true;
    }
    [PARENT_TITLE_PREFIX, CHILD_TITLE_PREFIX]
        .iter()
        .any(|prefix| {
            trimmed.starts_with(prefix)
                || trimmed == prefix.trim_end().trim_end_matches('-').trim_end()
        })
}

/// List sessions in one of the three scopes.
///
/// Ordering is `<sort column> DESC, id DESC` by default — `listGlobal`'s
/// `orderBy(desc(time_updated), desc(id))` (`session.ts:574`). The `id`
/// tie-break is what makes the order total: `time_updated` is a millisecond
/// clock reading, so two sessions created in the same millisecond would
/// otherwise come back in whatever order SQLite chose, and a keyset cursor over
/// an unstable order skips or repeats rows.
///
/// # Errors
///
/// [`DbError::Query`] if the read fails.
pub fn list(connection: &Connection, query: &ListQuery) -> Result<Vec<Session>, DbError> {
    let (sql, values) = list_sql(query);
    let mut statement = connection.prepare(&sql).map_err(open::map_error)?;
    let rows = statement
        .query_map(params_from_iter(values), from_row)
        .map_err(open::map_error)?;
    let mut sessions = Vec::new();
    for row in rows {
        sessions.push(row.map_err(open::map_error)?);
    }
    Ok(sessions)
}

/// List sessions and attach each one's project summary.
///
/// Two statements, as upstream does it: the sessions, then one `IN` lookup over
/// their distinct `project_id`s (`session.ts:578-595`). Not a join, so a
/// session whose project row is gone still comes back — with `project: None`.
///
/// # Errors
///
/// [`DbError::Query`] if either read fails.
pub fn list_global(
    connection: &Connection,
    query: &ListQuery,
) -> Result<Vec<GlobalSession>, DbError> {
    let sessions = list(connection, query)?;
    let summaries = project_summaries(connection, &sessions)?;
    Ok(sessions
        .into_iter()
        .map(|session| {
            let project = summaries
                .iter()
                .find(|summary| summary.id == session.project_id)
                .cloned();
            GlobalSession { session, project }
        })
        .collect())
}

/// The immediate children of one session, oldest first.
///
/// `session.ts:598-606` filters on `parent_id` with no ordering; a stable order
/// is added here so a caller that prints the tree gets the same output twice.
///
/// # Errors
///
/// [`DbError::Query`] if the read fails.
pub fn children(connection: &Connection, parent_id: &str) -> Result<Vec<Session>, DbError> {
    let sql = format!("SELECT {COLUMNS} FROM session WHERE parent_id = ?1 ORDER BY id ASC");
    let mut statement = connection.prepare(&sql).map_err(open::map_error)?;
    let rows = statement
        .query_map(params![parent_id], from_row)
        .map_err(open::map_error)?;
    let mut sessions = Vec::new();
    for row in rows {
        sessions.push(row.map_err(open::map_error)?);
    }
    Ok(sessions)
}

/// Every id in the subtree rooted at `id`, deepest first, `id` last.
///
/// This is the walk `remove` needs and the answer a prune or quota pass needs,
/// so it is public on its own. The traversal is iterative rather than recursive:
/// `parent_id` has no foreign key, so nothing in the schema prevents a cycle,
/// and a corrupted `a -> b -> a` pair would make a recursive walk loop until it
/// overflowed the stack. A visited set makes such a pair terminate instead.
///
/// The returned order is post-order — children before their parent, matching
/// `session.ts:619-622`, which recurses into every child before the parent's own
/// delete.
///
/// # Errors
///
/// [`DbError::Query`] if any read fails.
pub fn subtree(connection: &Connection, id: &str) -> Result<Vec<String>, DbError> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut order: Vec<String> = Vec::new();
    let mut stack: Vec<(String, bool)> = vec![(id.to_owned(), false)];

    while let Some((current, expanded)) = stack.pop() {
        if expanded {
            order.push(current);
            continue;
        }
        if !seen.insert(current.clone()) {
            continue;
        }
        stack.push((current.clone(), true));
        for child in child_ids(connection, &current)? {
            if !seen.contains(&child) {
                stack.push((child, false));
            }
        }
    }
    Ok(order)
}

/// Delete a session and everything below it, in one transaction.
///
/// Returns the removed ids, deepest first. The caller owns the transaction so
/// this composes with whatever else has to land atomically alongside it —
/// and so the whole subtree is one unit: a partial subtree delete would leave
/// exactly the orphans this function exists to prevent.
///
/// Per id, in this order:
///
/// 1. `DELETE FROM session`, which cascades `message`, `session_message`,
///    `session_input`, `session_context_epoch`, `session_share`, `agent_job`,
///    `work_plan`, `work_plan_archive`, `work_item`, and reflection delivery
///    rows, and reaches `part` through `message`;
/// 2. `DELETE FROM event`, keyed by `aggregate_id` (`event.ts:513-523`), which the
///    `event_sequence` sweep below also cascades to whenever the `foreign_keys`
///    pragma is on, and which is named anyway so this does not depend on it;
/// 3. the sweep over every session-keyed table no foreign key covers, which
///    [`crate::session_keys::uncascaded`] reads out of the live schema: today
///    `event_sequence`, `human_request`, `part`, `provider_retry_backoff`, and
///    `verification_receipt`.
///
/// The enumeration is not written down here, and it is not written down twice.
/// Sweeping four of the five such tables is what leaked the user's own text —
/// `human_request.payload` and `response` hold a question and the answer given to it —
/// for every deleted session, and the same list had to be, and was not, kept in step
/// with [`crate::prune`], which is the path retention actually takes. Both paths now
/// sweep the set SQLite reports, so a table added later with a session key and no
/// foreign key cannot land in only one of them.
/// `tests/session.rs::removing_a_session_sweeps_every_table_no_cascade_reaches` and
/// `tests/prune.rs::prune_delete_sweeps_every_session_keyed_table_no_cascade_reaches`
/// both derive their expectation from the schema and fail until such a table is seeded.
///
/// Background-job cancellation (`session.ts:618`) is deliberately absent: the
/// job registry is not in this crate. The returned ids are what a caller needs
/// to do it.
///
/// # Errors
///
/// [`DbError::NotFound`] when no row has that id, mirroring the `get` at
/// `session.ts:609`. [`DbError::Query`] if any statement fails.
pub fn remove(transaction: &Transaction<'_>, id: &str) -> Result<Vec<String>, DbError> {
    if find(transaction, id)?.is_none() {
        return Err(DbError::NotFound {
            table: TABLE.to_owned(),
            id: id.to_owned(),
        });
    }
    let removed = subtree(transaction, id)?;
    let uncascaded = crate::session_keys::uncascaded(transaction)?;
    for current in &removed {
        transaction
            .execute("DELETE FROM session WHERE id = ?1", params![current])
            .map_err(open::map_error)?;
        transaction
            .execute(
                "DELETE FROM event WHERE aggregate_id = ?1",
                params![current],
            )
            .map_err(open::map_error)?;
        crate::session_keys::sweep_one(transaction, &uncascaded, current)?;
    }
    Ok(removed)
}

/// The session table over a [`Pool`], for callers that do not want to manage
/// transactions themselves.
///
/// Reads take a pooled connection; writes go through [`Pool::transaction`],
/// which is `IMMEDIATE`, so a subtree delete is one atomic unit even with
/// another writer active.
#[derive(Debug, Clone, Copy)]
pub struct Store<'pool> {
    pool: &'pool Pool,
}

impl<'pool> Store<'pool> {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: &'pool Pool) -> Self {
        Self { pool }
    }

    /// The pool underneath.
    #[must_use]
    pub fn pool(&self) -> &'pool Pool {
        self.pool
    }

    /// See [`create`].
    ///
    /// # Errors
    ///
    /// Whatever [`create`] returns, plus [`DbError::Open`] if no connection
    /// could be obtained.
    pub fn create(&self, input: &SessionCreate) -> Result<Creation, DbError> {
        self.pool
            .transaction(|transaction| create(transaction, input))
    }

    /// See [`get`].
    ///
    /// # Errors
    ///
    /// Whatever [`get`] returns, plus [`DbError::Open`] if no connection could
    /// be obtained.
    pub fn get(&self, id: &str) -> Result<Session, DbError> {
        let connection = self.pool.get()?;
        get(&connection, id)
    }

    /// See [`find`].
    ///
    /// # Errors
    ///
    /// Whatever [`find`] returns, plus [`DbError::Open`] if no connection could
    /// be obtained.
    pub fn find(&self, id: &str) -> Result<Option<Session>, DbError> {
        let connection = self.pool.get()?;
        find(&connection, id)
    }

    /// See [`list`].
    ///
    /// # Errors
    ///
    /// Whatever [`list`] returns, plus [`DbError::Open`] if no connection could
    /// be obtained.
    pub fn list(&self, query: &ListQuery) -> Result<Vec<Session>, DbError> {
        let connection = self.pool.get()?;
        list(&connection, query)
    }

    /// See [`list_global`].
    ///
    /// # Errors
    ///
    /// Whatever [`list_global`] returns, plus [`DbError::Open`] if no connection
    /// could be obtained.
    pub fn list_global(&self, query: &ListQuery) -> Result<Vec<GlobalSession>, DbError> {
        let connection = self.pool.get()?;
        list_global(&connection, query)
    }

    /// See [`children`].
    ///
    /// # Errors
    ///
    /// Whatever [`children`] returns, plus [`DbError::Open`] if no connection
    /// could be obtained.
    pub fn children(&self, parent_id: &str) -> Result<Vec<Session>, DbError> {
        let connection = self.pool.get()?;
        children(&connection, parent_id)
    }

    /// See [`subtree`].
    ///
    /// # Errors
    ///
    /// Whatever [`subtree`] returns, plus [`DbError::Open`] if no connection
    /// could be obtained.
    pub fn subtree(&self, id: &str) -> Result<Vec<String>, DbError> {
        let connection = self.pool.get()?;
        subtree(&connection, id)
    }

    /// See [`touch`].
    ///
    /// # Errors
    ///
    /// Whatever [`touch`] returns, plus [`DbError::Open`] if no connection could
    /// be obtained.
    pub fn touch(&self, id: &str) -> Result<i64, DbError> {
        self.pool.transaction(|transaction| touch(transaction, id))
    }

    /// See [`touch_at`].
    ///
    /// # Errors
    ///
    /// Whatever [`touch_at`] returns, plus [`DbError::Open`] if no connection
    /// could be obtained.
    pub fn touch_at(&self, id: &str, millis: i64) -> Result<i64, DbError> {
        self.pool
            .transaction(|transaction| touch_at(transaction, id, millis))
    }

    /// See [`set_title`].
    ///
    /// # Errors
    ///
    /// Whatever [`set_title`] returns, plus [`DbError::Open`] if no connection
    /// could be obtained.
    pub fn set_title(&self, id: &str, title: &str) -> Result<i64, DbError> {
        self.pool
            .transaction(|transaction| set_title(transaction, id, title))
    }

    /// See [`set_metadata`].
    ///
    /// # Errors
    ///
    /// Whatever [`set_metadata`] returns, plus [`DbError::Open`] if no connection could
    /// be obtained.
    pub fn set_metadata(&self, id: &str, metadata: &str) -> Result<i64, DbError> {
        self.pool
            .transaction(|transaction| set_metadata(transaction, id, metadata))
    }

    pub fn switch_agent_at(
        &self,
        id: &str,
        message_id: &str,
        agent: &str,
        millis: i64,
    ) -> Result<(), DbError> {
        self.pool
            .transaction(|transaction| switch_agent_at(transaction, id, message_id, agent, millis))
    }

    pub fn switch_model_at(
        &self,
        id: &str,
        message_id: &str,
        model: &str,
        millis: i64,
    ) -> Result<(), DbError> {
        self.pool
            .transaction(|transaction| switch_model_at(transaction, id, message_id, model, millis))
    }

    pub fn stage_revert_at(
        &self,
        id: &str,
        message_id: &str,
        revert: &str,
        millis: i64,
    ) -> Result<(), DbError> {
        self.pool
            .transaction(|transaction| stage_revert_at(transaction, id, message_id, revert, millis))
    }

    pub fn clear_revert_at(&self, id: &str, millis: i64) -> Result<(), DbError> {
        self.pool
            .transaction(|transaction| clear_revert_at(transaction, id, millis))
    }

    pub fn commit_revert_at(&self, id: &str, millis: i64) -> Result<bool, DbError> {
        self.pool
            .transaction(|transaction| commit_revert_at(transaction, id, millis))
    }

    /// See [`remove`]. The whole subtree lands in one transaction.
    ///
    /// # Errors
    ///
    /// Whatever [`remove`] returns, plus [`DbError::Open`] if no connection
    /// could be obtained.
    pub fn remove(&self, id: &str) -> Result<Vec<String>, DbError> {
        self.pool.transaction(|transaction| remove(transaction, id))
    }
}

/// The `SELECT` a listing runs, and the bindings it expects, in order.
///
/// Split out of [`list`] so a caller that needs to wrap the same rows in
/// something else — [`crate::session_list`] joins them against `project` and
/// counts their messages — inherits this query's predicates, limit **and**
/// ordering instead of restating them. A second hand-written copy of the
/// `time_updated DESC, id DESC` tie-break is the kind of divergence that only
/// shows up as a paginated client seeing a row twice.
pub(crate) fn list_sql(query: &ListQuery) -> (String, Vec<Value>) {
    let (predicates, bindings) = filters(query);
    let sort = query.sort.column();
    let direction = query.direction.keyword();

    let mut sql = format!("SELECT {COLUMNS} FROM session");
    if !predicates.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&predicates.join(" AND "));
    }
    sql.push_str(&format!(" ORDER BY {sort} {direction}, id {direction}"));
    let mut values = bindings;
    if let Some(limit) = query.limit {
        sql.push_str(&format!(" LIMIT ?{}", values.len() + 1));
        values.push(Value::Integer(i64::from(limit)));
    }
    (sql, values)
}

/// Build the `WHERE` predicates and their bindings for a listing.
///
/// Returns positional SQL fragments already numbered against the bindings that
/// come back with them, so the caller only has to join them.
fn filters(query: &ListQuery) -> (Vec<String>, Vec<Value>) {
    let mut predicates: Vec<String> = Vec::new();
    let mut values: Vec<Value> = Vec::new();

    match &query.scope {
        ListScope::Directory { directory } => {
            values.push(Value::Text(directory.clone()));
            predicates.push(format!("directory = ?{}", values.len()));
        }
        ListScope::Project {
            project_id,
            subpath,
        } => {
            values.push(Value::Text(project_id.clone()));
            predicates.push(format!("project_id = ?{}", values.len()));
            // Upstream declares `subpath` on this arm of the union
            // (`core/src/session.ts:64-68`) and then never reads it
            // (`core/src/session.ts:268-303`), so a caller asking for one
            // directory's sessions silently receives the whole project's. The
            // predicate below is the fix, taking its prefix semantics from the
            // legacy path filter it corresponds to (`session.ts:969-984`):
            // the subpath itself, plus everything beneath it.
            //
            // Expressed with `substr` rather than `LIKE ? || '/%'` on purpose.
            // `LIKE` would read `_` and `%` in a directory name as wildcards —
            // a session under `a_b/` would match a subpath of `axb` — and
            // upstream's version has that bug because it interpolates the path
            // straight into the pattern. There is no index on `session.path`
            // either way, so the exact form costs nothing.
            //
            // An empty subpath filters nothing, matching upstream's
            // `if (input.path)` guard.
            if let Some(subpath) = subpath.as_ref().filter(|value| !value.is_empty()) {
                values.push(Value::Text(subpath.clone()));
                let slot = values.len();
                predicates.push(format!(
                    "(path = ?{slot} OR substr(path, 1, length(?{slot}) + 1) = ?{slot} || '/')"
                ));
            }
        }
        ListScope::Global => {}
    }

    if let Some(workspace_id) = &query.workspace_id {
        values.push(Value::Text(workspace_id.clone()));
        predicates.push(format!("workspace_id = ?{}", values.len()));
    }
    if query.roots {
        predicates.push(String::from("parent_id IS NULL"));
    }
    if let Some(search) = &query.search {
        values.push(Value::Text(format!("%{search}%")));
        predicates.push(format!("title LIKE ?{}", values.len()));
    }
    let sort = query.sort.column();
    if let Some(start) = query.start {
        values.push(Value::Integer(start));
        predicates.push(format!("{sort} >= ?{}", values.len()));
    }
    if let Some(cursor) = query.cursor {
        values.push(Value::Integer(cursor));
        predicates.push(format!("{sort} < ?{}", values.len()));
    }
    match query.archived {
        ArchivedFilter::Any => {}
        ArchivedFilter::Active => predicates.push(String::from("time_archived IS NULL")),
        ArchivedFilter::Archived => predicates.push(String::from("time_archived IS NOT NULL")),
    }

    (predicates, values)
}

/// The distinct project summaries behind a set of sessions.
fn project_summaries(
    connection: &Connection,
    sessions: &[Session],
) -> Result<Vec<ProjectSummary>, DbError> {
    let mut ids: Vec<String> = Vec::new();
    for session in sessions {
        if !ids.contains(&session.project_id) {
            ids.push(session.project_id.clone());
        }
    }
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = (1..=ids.len())
        .map(|slot| format!("?{slot}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT id, name, worktree FROM project WHERE id IN ({placeholders})");
    let mut statement = connection.prepare(&sql).map_err(open::map_error)?;
    let rows = statement
        .query_map(params_from_iter(ids.into_iter().map(Value::Text)), |row| {
            Ok(ProjectSummary {
                id: row.get(0)?,
                name: row.get(1)?,
                worktree: row.get(2)?,
            })
        })
        .map_err(open::map_error)?;
    let mut summaries = Vec::new();
    for row in rows {
        summaries.push(row.map_err(open::map_error)?);
    }
    Ok(summaries)
}

/// The ids of one session's immediate children.
fn child_ids(connection: &Connection, parent_id: &str) -> Result<Vec<String>, DbError> {
    let mut statement = connection
        .prepare("SELECT id FROM session WHERE parent_id = ?1 ORDER BY id ASC")
        .map_err(open::map_error)?;
    let rows = statement
        .query_map(params![parent_id], |row| row.get::<_, String>(0))
        .map_err(open::map_error)?;
    let mut ids = Vec::new();
    for row in rows {
        ids.push(row.map_err(open::map_error)?);
    }
    Ok(ids)
}

/// Decode one row selected with [`COLUMNS`], in that column order.
pub(crate) fn from_row(row: &Row<'_>) -> rusqlite::Result<Session> {
    let summary_additions: Option<i64> = row.get(10)?;
    let summary_deletions: Option<i64> = row.get(11)?;
    let summary_files: Option<i64> = row.get(12)?;
    let summary_diffs: Option<String> = row.get(13)?;
    let summary =
        if summary_additions.is_some() || summary_deletions.is_some() || summary_files.is_some() {
            Some(Summary {
                additions: summary_additions.unwrap_or(0),
                deletions: summary_deletions.unwrap_or(0),
                files: summary_files.unwrap_or(0),
                diffs: summary_diffs,
            })
        } else {
            None
        };

    Ok(Session {
        id: row.get(0)?,
        project_id: row.get(1)?,
        workspace_id: row.get(2)?,
        parent_id: row.get(3)?,
        slug: row.get(4)?,
        directory: row.get(5)?,
        path: row.get(6)?,
        title: row.get(7)?,
        version: row.get(8)?,
        share_url: row.get(9)?,
        summary,
        metadata: row.get(14)?,
        usage: SessionUsage {
            cost: row.get(15)?,
            tokens: Tokens {
                input: row.get(16)?,
                output: row.get(17)?,
                reasoning: row.get(18)?,
                cache_read: row.get(19)?,
                cache_write: row.get(20)?,
            },
            last_prompt_tokens: row.get(21)?,
            context_limit: row.get(22)?,
            accounting: row
                .get::<_, Option<String>>(23)?
                .as_deref()
                .and_then(TokenAccounting::parse),
            known: row.get(24)?,
            estimated_pending_prompt_tokens: row.get(25)?,
            last_confirmed_at: row.get(26)?,
            failed_turns: row.get(27)?,
            last_failed_at: row.get(28)?,
        },
        revert: row.get(29)?,
        permission: row.get(30)?,
        agent: row.get(31)?,
        model: row.get(32)?,
        time_created: row.get(33)?,
        time_updated: row.get(34)?,
        time_compacting: row.get(35)?,
        time_archived: row.get(36)?,
    })
}

fn unix_milliseconds() -> Result<i64, DbError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| DbError::Query {
            source: Box::new(error),
        })?;
    i64::try_from(elapsed.as_millis()).map_err(|error| DbError::Query {
        source: Box::new(error),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bindings(query: &ListQuery) -> (Vec<String>, Vec<Value>) {
        filters(query)
    }

    #[test]
    fn the_global_scope_adds_no_predicate() {
        let (predicates, values) = bindings(&ListQuery::global());
        assert!(predicates.is_empty(), "{predicates:?}");
        assert!(values.is_empty(), "{values:?}");
    }

    #[test]
    fn the_directory_scope_matches_the_directory_exactly() {
        let (predicates, values) = bindings(&ListQuery::directory("/srv/app"));
        assert_eq!(predicates, vec![String::from("directory = ?1")]);
        assert_eq!(values, vec![Value::Text(String::from("/srv/app"))]);
    }

    #[test]
    fn the_project_scope_without_a_subpath_matches_only_the_project() {
        let (predicates, values) = bindings(&ListQuery::project("prj_a"));
        assert_eq!(predicates, vec![String::from("project_id = ?1")]);
        assert_eq!(values, vec![Value::Text(String::from("prj_a"))]);
    }

    #[test]
    fn the_project_scope_with_a_subpath_adds_a_prefix_predicate() {
        let query = ListQuery::project("prj_a").with_subpath("packages/core");
        let (predicates, values) = bindings(&query);
        assert_eq!(
            predicates,
            vec![
                String::from("project_id = ?1"),
                String::from("(path = ?2 OR substr(path, 1, length(?2) + 1) = ?2 || '/')"),
            ]
        );
        assert_eq!(
            values,
            vec![
                Value::Text(String::from("prj_a")),
                Value::Text(String::from("packages/core")),
            ]
        );
        assert!(query.subpath_applies());
    }

    #[test]
    fn an_empty_subpath_filters_nothing() {
        let query = ListQuery::project("prj_a").with_subpath("");
        let (predicates, _) = bindings(&query);
        assert_eq!(predicates, vec![String::from("project_id = ?1")]);
        assert!(!query.subpath_applies());
    }

    #[test]
    fn a_subpath_is_not_accepted_on_the_directory_scope() {
        let query = ListQuery::directory("/srv/app").with_subpath("packages/core");
        assert_eq!(
            query.scope,
            ListScope::Directory {
                directory: String::from("/srv/app")
            }
        );
        assert!(!query.subpath_applies());
    }

    #[test]
    fn a_subpath_is_not_accepted_on_the_global_scope() {
        let query = ListQuery::global().with_subpath("packages/core");
        assert_eq!(query.scope, ListScope::Global);
        assert!(!query.subpath_applies());
    }

    #[test]
    fn every_narrowing_filter_numbers_its_own_binding() {
        let query = ListQuery {
            scope: ListScope::Project {
                project_id: String::from("prj_a"),
                subpath: Some(String::from("pkg")),
            },
            workspace_id: Some(String::from("wrk_a")),
            search: Some(String::from("review")),
            roots: true,
            start: Some(10),
            cursor: Some(90),
            archived: ArchivedFilter::Active,
            sort: SessionSort::Updated,
            direction: SortDirection::Descending,
            limit: Some(25),
        };
        let (predicates, values) = bindings(&query);
        assert_eq!(
            predicates,
            vec![
                String::from("project_id = ?1"),
                String::from("(path = ?2 OR substr(path, 1, length(?2) + 1) = ?2 || '/')"),
                String::from("workspace_id = ?3"),
                String::from("parent_id IS NULL"),
                String::from("title LIKE ?4"),
                String::from("time_updated >= ?5"),
                String::from("time_updated < ?6"),
                String::from("time_archived IS NULL"),
            ]
        );
        assert_eq!(
            values,
            vec![
                Value::Text(String::from("prj_a")),
                Value::Text(String::from("pkg")),
                Value::Text(String::from("wrk_a")),
                Value::Text(String::from("%review%")),
                Value::Integer(10),
                Value::Integer(90),
            ]
        );
    }

    #[test]
    fn the_created_sort_bounds_the_created_column() {
        let query = ListQuery {
            start: Some(5),
            ..ListQuery::global().created_order()
        };
        let (predicates, _) = bindings(&query);
        assert_eq!(predicates, vec![String::from("time_created >= ?1")]);
        assert_eq!(query.sort.column(), "time_created");
    }

    #[test]
    fn the_archived_filter_defaults_to_hiding_nothing() {
        assert_eq!(ArchivedFilter::default(), ArchivedFilter::Any);
        let (predicates, _) = bindings(&ListQuery::global());
        assert!(predicates.is_empty(), "{predicates:?}");
        let (archived, _) = bindings(&ListQuery {
            archived: ArchivedFilter::Archived,
            ..ListQuery::global()
        });
        assert_eq!(archived, vec![String::from("time_archived IS NOT NULL")]);
    }

    #[test]
    fn the_default_sort_is_updated_descending() {
        let query = ListQuery::default();
        assert_eq!(query.sort, SessionSort::Updated);
        assert_eq!(query.direction, SortDirection::Descending);
        assert_eq!(query.sort.column(), "time_updated");
        assert_eq!(query.direction.keyword(), "DESC");
        assert_eq!(SortDirection::Ascending.keyword(), "ASC");
    }

    #[test]
    fn the_default_limit_is_absent_rather_than_a_hundred() {
        assert_eq!(ListQuery::default().limit, None);
        assert_eq!(UPSTREAM_LIST_LIMIT, 100);
    }

    #[test]
    fn the_default_title_prefix_follows_the_parent() {
        assert_eq!(SessionCreate::default_title_prefix(None), "New session - ");
        assert_eq!(
            SessionCreate::default_title_prefix(Some("ses_parent")),
            "Child session - "
        );
    }

    #[test]
    fn a_model_reference_uses_the_session_spelling_and_omits_the_variant() {
        assert_eq!(
            model_reference("anthropic", "claude-sonnet-4-5"),
            r#"{"id":"claude-sonnet-4-5","providerID":"anthropic"}"#
        );
        let quoted = model_reference("a\"b", "c\\d");
        let parsed: serde_json::Value =
            serde_json::from_str(&quoted).expect("a quoted id still produces JSON");
        assert_eq!(parsed["id"], "c\\d");
        assert_eq!(parsed["providerID"], "a\"b");
    }

    #[test]
    fn a_model_reference_with_a_variant_adds_only_the_optional_key() {
        assert_eq!(
            model_reference_with_variant("anthropic", "claude-sonnet-4-5", Some("high")),
            r#"{"id":"claude-sonnet-4-5","providerID":"anthropic","variant":"high"}"#
        );
        assert_eq!(
            model_reference_with_variant("anthropic", "claude-sonnet-4-5", None),
            model_reference("anthropic", "claude-sonnet-4-5"),
            "an absent variant must stay byte-identical to the two-key reference"
        );
    }

    #[test]
    fn decoding_accepts_both_persisted_shapes_and_refuses_garbage() {
        assert_eq!(
            decode_model_reference(r#"{"id":"claude-sonnet-4-5","providerID":"anthropic"}"#),
            Some(PersistedModel {
                provider_id: "anthropic".to_owned(),
                model_id: "claude-sonnet-4-5".to_owned(),
                variant: None,
            })
        );
        assert_eq!(
            decode_model_reference(
                r#"{"id":"claude-sonnet-4-5","providerID":"anthropic","variant":"xhigh"}"#
            ),
            Some(PersistedModel {
                provider_id: "anthropic".to_owned(),
                model_id: "claude-sonnet-4-5".to_owned(),
                variant: Some("xhigh".to_owned()),
            })
        );
        assert_eq!(
            decode_model_reference(
                r#"{"id":"claude-sonnet-4-5","providerID":"anthropic","variant":null}"#
            )
            .and_then(|model| model.variant),
            None,
            "an explicit null variant is the same as an omitted one"
        );
        // The plain string a format-7 writer stored for `ses_fixture_0001`.
        let legacy = decode_model_reference("anthropic/claude-sonnet-4").expect("legacy string");
        assert_eq!(legacy.provider_id, "anthropic");
        assert_eq!(legacy.model_id, "claude-sonnet-4");
        assert_eq!(legacy.variant, None);
        assert_eq!(legacy.qualified(), "anthropic/claude-sonnet-4");
        // A message-table spelling has no `id`, so it is not a session model.
        assert_eq!(
            decode_model_reference(r#"{"modelID":"claude-sonnet-4-5","providerID":"anthropic"}"#),
            None
        );
        for garbage in [
            "",
            "   ",
            "not-qualified",
            "/model",
            "provider/",
            "[]",
            "42",
            "{}",
        ] {
            assert_eq!(decode_model_reference(garbage), None, "{garbage:?}");
        }
    }
}

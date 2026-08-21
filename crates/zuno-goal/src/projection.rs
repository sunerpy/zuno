//! The human-editable Markdown projection of the goal, and the conflict rule.
//!
//! # Why a document exists at all
//!
//! Codex deliberately has none: its goal is reachable through goal controls and
//! tools, with no file to open. Zuno adds a rendered Markdown projection at
//! `.zuno/goal/<sessionID>.md` that a human can read and edit.
//!
//! The projection is project-local when the project is under version control and
//! uses the global data directory otherwise. [`document_path`] makes the choice
//! explicit: a project-local file is easy to find, while a non-repository has no
//! managed project directory for it.
//!
//! # The conflict rule, which is the whole point of this module
//!
//! Two writers can touch the same goal: SQL, and whoever has the file open in an
//! editor. The split is fixed and not configurable:
//!
//! | field | authority | on a hand edit |
//! |---|---|---|
//! | the objective text | **the document** | adopted on the next turn |
//! | `status` | **SQL** | rejected, and the rejection is written into the document |
//! | `token_budget`, `tokens_used`, `time_used_seconds` | **SQL** | rejected the same way |
//! | `session_id`, `goal_id`, the timestamps | **SQL** | rejected the same way |
//! | the checklist | **SQL** — it is a projection, not an input | rejected the same way |
//!
//! The asymmetry is deliberate. The objective is *prose a human wrote*, so the
//! human's copy is the better one. Status is a *decision about whether the run
//! continues*, taken on evidence the document does not hold — and a document that
//! could set it would let an editor left open on a stale copy resurrect a
//! completed goal by saving. [`crate::status`] already refuses that from the
//! model; this module refuses it from the filesystem.
//!
//! **No rejected edit disappears quietly.** Every one of them is rendered back
//! into the document under `## Rejected edits`, naming the field, what it was set
//! to, who owns it and what the database actually says. A document that silently
//! reverted a user's edit would train the user to distrust the file, which is
//! worse than not having one.
//!
//! # Why the render is atomic, and why it does not `fsync`
//!
//! A temporary file in the destination's own directory, then a rename. Same
//! filesystem, so the rename is atomic, so a concurrent reader always observes
//! one complete document and no lock is needed. This is the approach
//! `zuno-memory/src/store.rs` settled on (todo 98) and the reasoning is recorded
//! with it; the helper there is private to that crate, so this is the same
//! technique rather than the same function.
//!
//! Neither implementation calls `sync_all`. For a *projection* that is the right
//! trade: the file is derived state, so a render lost to a power cut is
//! regenerated from SQL on the next material change, and paying an fsync on every
//! token-count update would be a real cost buying nothing SQL does not already
//! guarantee.
//!
//! # Why this module owns no watcher
//!
//! The plan says to watch the file, and `zuno-watch` (todo 50) is the crate for it.
//! It is not constructed here, for two measured reasons:
//!
//! 1. **`zuno-watch`'s default decision does not watch the project directory.**
//!    Without `ZUNO_EXPERIMENTAL_FILEWATCHER` the flag resolution yields
//!    `Decision::VcsOnly`, which watches only the VCS directory. A watcher built
//!    inside this module would therefore be silently inert in the default
//!    configuration, and forcing it on from here would override a user's flag.
//! 2. **`zuno-watch` has no way to ignore an event you are about to cause.** There
//!    is no suppress token and no per-path exemption beyond the static ignore
//!    filter, so the self-render problem has to be solved on this side anyway.
//!
//! So the seam is typed on `zuno-watch`'s own [`FileEvent`]: whoever already runs a
//! [`zuno_watch::Watcher`] routes matching events into
//! [`GoalProjection::ingest_event`]. One watcher, in the crate that owns
//! watching.
//!
//! # Breaking the write-then-watch feedback loop
//!
//! This module writes the file *and* reads events for it, so its own render would
//! otherwise look exactly like a user edit and be re-ingested forever.
//! [`GoalProjection`] retains the exact bytes of its last render and compares the
//! file against them: byte-identical means "this is our own write" and the ingest
//! stops at [`Ingest::OwnRender`] without touching SQL and without rewriting the
//! file.
//!
//! Retaining the bytes rather than an mtime-and-length stamp is the cheaper option
//! *here*, which is the opposite of `zuno-memory`'s conclusion — and the difference
//! is informative. `zuno-memory` compares a file against a version it no longer
//! holds, so it must reconstruct a fingerprint; a projection has just rendered the
//! bytes it is about to compare, so exact equality costs one `String` and admits
//! no same-size-within-one-timestamp-tick false negative. It also makes a save
//! that changed nothing correctly read as "no edit".
//!
//! # Keeping the document out of git
//!
//! The projection is derived, per-session, and churns on every material change, so
//! it does not belong in a repository. [`GITIGNORE_SNIPPET`] is the recommended
//! text, and it lives here as a constant because the project has no
//! recommended-gitignore file for it to be appended to — searching the workspace
//! for one turns up only gitignore *parsing* (`zuno-watch`, `zuno-search`,
//! `zuno-snapshot`) and the repository's own `.gitignore`. Whoever adds user
//! documentation or an `init` command should emit this constant rather than
//! retyping the path.

use crate::error::GoalError;
use crate::status::GoalStatus;
use crate::store::{Goal, GoalStore};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};
use zuno_watch::{ChangeKind, FileEvent};

pub use zuno_paths::PROJECT_DIRECTORY;

/// The subdirectory of [`PROJECT_DIRECTORY`] holding goal documents.
pub const GOAL_DIRECTORY: &str = "goal";

/// What to add to a repository's `.gitignore` for the goal projection.
///
/// See the module docs for why this is a constant rather than an entry in an
/// existing snippet file.
pub const GITIGNORE_SNIPPET: &str = "\
# Zuno renders the authoritative goal to a human-editable Markdown document,
# one per session. It is derived from the goal database and rewritten on every
# material change, so it is local working state rather than source.
.zuno/goal/
";

/// Opens the region of the document whose contents the human owns.
pub const OBJECTIVE_BEGIN: &str = "<!-- goal:objective:begin -->";

/// Closes the region of the document whose contents the human owns.
pub const OBJECTIVE_END: &str = "<!-- goal:objective:end -->";

/// A field the document projects and SQL owns.
///
/// Exported so a test can walk the whole set rather than the handful of fields
/// whoever wrote it remembered, and so a future field cannot be added without
/// deciding whether it is guarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// The session the goal belongs to.
    SessionId,
    /// The goal instance.
    GoalId,
    /// Whether, and why, the agent should keep going.
    Status,
    /// When the goal instance was created.
    CreatedAtMs,
    /// When it last changed.
    UpdatedAtMs,
    /// The token ceiling.
    TokenBudget,
    /// Tokens spent against this goal instance.
    TokensUsed,
    /// Tokens left before the budget flips the status.
    TokensRemaining,
    /// Wall-clock seconds spent against this goal instance.
    TimeUsedSeconds,
}

impl Field {
    /// Every projected field, in the order the document renders them.
    pub const ALL: [Self; 9] = [
        Self::SessionId,
        Self::GoalId,
        Self::Status,
        Self::CreatedAtMs,
        Self::UpdatedAtMs,
        Self::TokenBudget,
        Self::TokensUsed,
        Self::TokensRemaining,
        Self::TimeUsedSeconds,
    ];

    /// The key the document labels this field with.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::SessionId => "session_id",
            Self::GoalId => "goal_id",
            Self::Status => "status",
            Self::CreatedAtMs => "created_at_ms",
            Self::UpdatedAtMs => "updated_at_ms",
            Self::TokenBudget => "token_budget",
            Self::TokensUsed => "tokens_used",
            Self::TokensRemaining => "tokens_remaining",
            Self::TimeUsedSeconds => "time_used_seconds",
        }
    }

    /// How a rejection message refers to this field's owner.
    ///
    /// Grouped rather than per-field because "the counters are the system's to
    /// set" is the sentence a user needs; "`tokens_remaining` is the system's to
    /// set" invites them to try `tokens_used` instead.
    #[must_use]
    pub fn noun(self) -> &'static str {
        self.owner_parts().0
    }

    /// The refusal an edit to this field earns.
    ///
    /// The only way to build one, so a plural noun can never be paired with a
    /// singular verb — the bug the first draft of [`RejectedEdit::message`] had.
    #[must_use]
    pub fn owner(self) -> Refusal {
        let (noun, plural) = self.owner_parts();
        Refusal::SystemOwned { noun, plural }
    }

    fn owner_parts(self) -> (&'static str, bool) {
        match self {
            Self::SessionId => ("the session id", false),
            Self::GoalId => ("the goal id", false),
            Self::Status => ("the status", false),
            Self::CreatedAtMs | Self::UpdatedAtMs => ("the timestamps", true),
            Self::TokenBudget => ("the token budget", false),
            Self::TokensUsed | Self::TokensRemaining | Self::TimeUsedSeconds => {
                ("the counters", true)
            }
        }
    }

    /// This field's value for `goal`, rendered as the document renders it.
    #[must_use]
    pub fn value(self, goal: &Goal) -> String {
        match self {
            Self::SessionId => goal.session_id.clone(),
            Self::GoalId => goal.goal_id.clone(),
            Self::Status => goal.status.as_str().to_owned(),
            Self::CreatedAtMs => goal.created_at_ms.to_string(),
            Self::UpdatedAtMs => goal.updated_at_ms.to_string(),
            Self::TokenBudget => goal
                .token_budget
                .map_or_else(|| "none".to_owned(), |budget| budget.to_string()),
            Self::TokensUsed => goal.tokens_used.to_string(),
            Self::TokensRemaining => goal
                .tokens_remaining()
                .map_or_else(|| "unbounded".to_owned(), |tokens| tokens.to_string()),
            Self::TimeUsedSeconds => goal.time_used_seconds.to_string(),
        }
    }
}

/// A checklist line, which is a projection of state and never an input.
///
/// Ticking one is the edit the whole conflict rule exists to refuse: `complete`
/// is the checkbox a stale editor would use to declare a run finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// The agent should keep working toward the objective.
    Active,
    /// The token budget has room left.
    WithinBudget,
    /// The model reported the objective met.
    Complete,
}

impl Check {
    /// Every checklist line, in the order the document renders them.
    pub const ALL: [Self; 3] = [Self::Active, Self::WithinBudget, Self::Complete];

    /// The key the document labels this line with.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::WithinBudget => "within_budget",
            Self::Complete => "complete",
        }
    }

    /// What the line says next to its checkbox.
    #[must_use]
    pub fn prose(self) -> &'static str {
        match self {
            Self::Active => "the agent should keep working toward the objective",
            Self::WithinBudget => "the token budget has room left",
            Self::Complete => "the model reported the objective met",
        }
    }

    /// Whether this line is ticked for `goal`.
    #[must_use]
    pub fn state(self, goal: &Goal) -> bool {
        match self {
            Self::Active => goal.status.is_active(),
            Self::WithinBudget => !goal.is_over_budget(),
            Self::Complete => goal.status == GoalStatus::Complete,
        }
    }

    fn rendered(self, goal: &Goal) -> &'static str {
        if self.state(goal) { "[x]" } else { "[ ]" }
    }
}

/// What in the document was edited, for a rejection message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edited {
    /// A `- \`key\`: value` line.
    Field(Field),
    /// A checklist checkbox.
    Check(Check),
    /// The objective region, when what it now holds cannot be stored.
    Objective,
}

impl Edited {
    /// How a rejection message names the thing that was edited.
    #[must_use]
    pub fn subject(self) -> String {
        match self {
            Self::Field(field) => format!("`{}`", field.key()),
            Self::Check(check) => format!("the `{}` checklist item", check.key()),
            Self::Objective => "`objective`".to_owned(),
        }
    }
}

/// Why an edit was not applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The field belongs to SQL, and the document may only display it.
    SystemOwned {
        /// How to refer to the owning group, from [`Field::noun`].
        noun: &'static str,
        /// Whether `noun` takes a plural verb. Build through [`Field::owner`].
        plural: bool,
    },
    /// The document owns this value but the value itself cannot be stored.
    Unstorable {
        /// The reason, phrased to complete "…, but {reason}".
        reason: String,
    },
}

/// One edit the conflict rule refused, and everything needed to explain it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedEdit {
    /// What the user changed.
    pub edited: Edited,
    /// What they changed it to.
    pub attempted: String,
    /// What the goal database says instead.
    pub actual: String,
    /// Why it did not take.
    pub refusal: Refusal,
}

impl RejectedEdit {
    /// The sentence rendered into the document under `## Rejected edits`.
    ///
    /// A tested artifact, not prose: it is the only place a user learns why their
    /// save did not take, so it names all four things they need — what they
    /// edited, what they set it to, who owns it, and what the value really is.
    #[must_use]
    pub fn message(&self) -> String {
        let subject = self.edited.subject();
        let Self {
            attempted, actual, ..
        } = self;
        match &self.refusal {
            Refusal::SystemOwned { noun, plural } => {
                let copula = if *plural { "are" } else { "is" };
                format!(
                    "- {subject} was edited to `{attempted}`, but {noun} {copula} the system's \
                     to set, not the document's; the goal database still says `{actual}`."
                )
            }
            Refusal::Unstorable { reason } => format!(
                "- {subject} was edited to `{attempted}`, but {reason}; \
                 the goal database still says `{actual}`."
            ),
        }
    }
}

/// The notes a render carries in addition to the goal's own state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Notes {
    /// Edits the conflict rule refused on the most recent ingest.
    pub rejected: Vec<RejectedEdit>,
    /// Where an unparsable document was preserved before being rebuilt.
    pub salvaged: Option<PathBuf>,
}

impl Notes {
    /// Whether there is anything to tell the user.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rejected.is_empty() && self.salvaged.is_none()
    }
}

/// A document that parsed completely.
///
/// "Completely" is the point: [`parse`] returns `None` unless the objective region
/// and *every* projected field and checklist key are present, so a caller that
/// holds one of these has proven it did not read a half-written file. That is what
/// makes it usable as the assertion in an atomicity test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    /// The contents of the objective region, trimmed.
    pub objective: String,
    /// Every `- \`key\`: value` line, by key.
    pub fields: BTreeMap<&'static str, String>,
    /// Every checklist line's state, by key.
    pub checks: BTreeMap<&'static str, bool>,
}

/// Where the goal document for `session_id` belongs.
///
/// `worktree` is `Some` when the project is under version control, mirroring the
/// oracle's choice for plans (`session.ts:331-335`); `None` puts the document in
/// the global data directory, because a project that is not a repository has no
/// project-local place that is safe to write.
///
/// Returns `None` for a session id that is not a single ordinary path component.
/// The id reaches this crate from a caller and becomes part of a path here, so
/// `../../etc/x` has to be refused rather than joined — the same reasoning as
/// [`crate::spill::objective_pointer_path`], which validates instead of parsing for the
/// same reason.
///
/// The check is on the **id**, not on the filename built from it: appending `.md`
/// turns `..` into the perfectly legal `...md`, so validating the derived name
/// accepts exactly the input that most needs refusing.
#[must_use]
pub fn document_path(worktree: Option<&Path>, session_id: &str) -> Option<PathBuf> {
    let session_id = session_id.trim();
    let mut components = Path::new(session_id).components();
    match components.next() {
        Some(std::path::Component::Normal(first)) if first == std::ffi::OsStr::new(session_id) => {}
        _ => return None,
    }
    if components.next().is_some() {
        return None;
    }
    let file = format!("{session_id}.md");
    Some(match worktree {
        Some(worktree) => worktree
            .join(PROJECT_DIRECTORY)
            .join(GOAL_DIRECTORY)
            .join(file),
        None => zuno_paths::data().join(GOAL_DIRECTORY).join(file),
    })
}

/// Render the authoritative goal as the document a human reads and edits.
///
/// Deterministic in `(goal, notes)`, with no clock and no filesystem access, so
/// two renders of the same state are byte-identical — which is what lets
/// [`GoalProjection`] recognise its own writes by comparison.
#[must_use]
pub fn render(goal: &Goal, notes: &Notes) -> String {
    let mut out = String::with_capacity(2_048);
    out.push_str(HEADER);

    out.push_str("\n## Objective\n\n");
    out.push_str(OBJECTIVE_BEGIN);
    out.push('\n');
    out.push_str(goal.objective.trim());
    out.push('\n');
    out.push_str(OBJECTIVE_END);
    out.push('\n');

    out.push_str("\n## State\n\n");
    for field in [
        Field::SessionId,
        Field::GoalId,
        Field::Status,
        Field::CreatedAtMs,
        Field::UpdatedAtMs,
    ] {
        push_field(&mut out, field, goal);
    }

    out.push_str("\n## Budget\n\n");
    for field in [
        Field::TokenBudget,
        Field::TokensUsed,
        Field::TokensRemaining,
        Field::TimeUsedSeconds,
    ] {
        push_field(&mut out, field, goal);
    }

    out.push_str("\n## Checklist\n\n");
    for check in Check::ALL {
        out.push_str("- ");
        out.push_str(check.rendered(goal));
        out.push_str(" `");
        out.push_str(check.key());
        out.push_str("`: ");
        out.push_str(check.prose());
        out.push('\n');
    }

    out.push_str("\n## Rejected edits\n\n");
    if notes.is_empty() {
        out.push_str("_Nothing has been rejected._\n");
    } else {
        if let Some(backup) = &notes.salvaged {
            out.push_str(&format!(
                "This document could not be parsed, so it was rebuilt from the goal \
                 database. Your version was kept at `{}`.\n",
                backup.display()
            ));
            if !notes.rejected.is_empty() {
                out.push('\n');
            }
        }
        if !notes.rejected.is_empty() {
            out.push_str("The last turn did not apply these edits.\n\n");
            for rejected in &notes.rejected {
                out.push_str(&rejected.message());
                out.push('\n');
            }
        }
    }
    out
}

/// Read a document back, or refuse it as incomplete.
///
/// Returns `None` when the objective region is missing or any projected field or
/// checklist key is absent — see [`Document`] for why the strictness is the
/// feature.
///
/// The objective region is taken from the *first* opening marker to the *last*
/// closing one, so an objective that itself contains a closing marker still round
/// trips: [`render`] always emits exactly one closing marker and always last.
#[must_use]
pub fn parse(text: &str) -> Option<Document> {
    let start = text.find(OBJECTIVE_BEGIN)? + OBJECTIVE_BEGIN.len();
    let end = text.rfind(OBJECTIVE_END)?;
    let objective = text.get(start..end)?.trim().to_owned();

    let mut fields = BTreeMap::new();
    let mut checks = BTreeMap::new();
    for line in text.lines().map(str::trim) {
        if let Some((field, value)) = field_line(line) {
            fields.insert(field.key(), value);
        } else if let Some((check, state)) = check_line(line) {
            checks.insert(check.key(), state);
        }
    }

    if fields.len() != Field::ALL.len() || checks.len() != Check::ALL.len() {
        return None;
    }
    Some(Document {
        objective,
        fields,
        checks,
    })
}

/// One session's goal document: rendering it, and re-ingesting what a human did to it.
///
/// Holds the bytes of its most recent render so it can tell its own write apart
/// from a user's edit. See the module docs for why that, and not a stamp.
#[derive(Debug)]
pub struct GoalProjection {
    path: PathBuf,
    session_id: String,
    last: Mutex<Option<LastRender>>,
}

#[derive(Debug, Clone)]
struct LastRender {
    document: String,
    goal: Goal,
}

/// What one ingest did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ingest {
    /// The session has no goal, so the document is not this crate's to interpret.
    NoGoal,
    /// The document was missing and has been rendered from SQL.
    Restored,
    /// The file is byte-identical to the last render: our own write, not an edit.
    OwnRender,
    /// The document did not parse. It was preserved and then rebuilt from SQL.
    Salvaged {
        /// Where the unparsable bytes were kept.
        backup: PathBuf,
    },
    /// The document was read and the conflict rule applied to it.
    Applied {
        /// The objective adopted into SQL, when the document changed it.
        adopted: Option<String>,
        /// Every edit the conflict rule refused.
        rejected: Vec<RejectedEdit>,
    },
}

impl Ingest {
    /// Whether SQL changed as a result of this ingest.
    #[must_use]
    pub fn adopted(&self) -> Option<&str> {
        match self {
            Self::Applied { adopted, .. } => adopted.as_deref(),
            _ => None,
        }
    }

    /// Every edit this ingest refused.
    #[must_use]
    pub fn rejected(&self) -> &[RejectedEdit] {
        match self {
            Self::Applied { rejected, .. } => rejected,
            _ => &[],
        }
    }
}

impl GoalProjection {
    /// Bind a projection to the document for `session_id` under `worktree`.
    ///
    /// Returns `None` when [`document_path`] refuses the session id.
    #[must_use]
    pub fn new(worktree: Option<&Path>, session_id: &str) -> Option<Self> {
        Some(Self::at(document_path(worktree, session_id)?, session_id))
    }

    /// Bind a projection to an explicit path.
    ///
    /// For a caller that has already resolved the location, and for tests.
    #[must_use]
    pub fn at(path: PathBuf, session_id: &str) -> Self {
        Self {
            path,
            session_id: session_id.to_owned(),
            last: Mutex::new(None),
        }
    }

    /// The document this projection renders.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The session whose goal this projects.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Whether a watch event names this document.
    #[must_use]
    pub fn matches(&self, path: &Path) -> bool {
        path == self.path
    }

    /// Render `goal` and replace the document atomically.
    ///
    /// Call this on every material change. Records the rendered bytes so the
    /// watch event this write causes is recognised as our own.
    ///
    /// # Errors
    ///
    /// [`GoalError::Document`] when the directory, the temporary file or the
    /// rename fails.
    pub fn write(&self, goal: &Goal) -> Result<(), GoalError> {
        self.write_notes(goal, &Notes::default())
    }

    /// Render `goal` together with `notes` and replace the document atomically.
    ///
    /// # Errors
    ///
    /// [`GoalError::Document`] when the directory, the temporary file or the
    /// rename fails.
    pub fn write_notes(&self, goal: &Goal, notes: &Notes) -> Result<(), GoalError> {
        let document = render(goal, notes);
        write_atomic(&self.path, &document)?;
        *self.lock() = Some(LastRender {
            document,
            goal: goal.clone(),
        });
        Ok(())
    }

    /// Route one watch event, ingesting only what this document's events mean.
    ///
    /// A `Unlink` restores the document, because the goal still exists and a
    /// deleted projection is a stale projection; `Add` and `Change` both mean
    /// "read it", which is the property that makes `zuno-watch`'s coalescing safe
    /// to consume (`zuno-watch/src/debounce.rs`).
    ///
    /// # Errors
    ///
    /// [`GoalError::Db`] when the goal cannot be read or the objective cannot be
    /// adopted, and [`GoalError::Document`] on an I/O failure.
    pub fn ingest_event(&self, store: &GoalStore, event: &FileEvent) -> Result<Ingest, GoalError> {
        if !self.matches(&event.path) {
            return Ok(Ingest::NoGoal);
        }
        match event.kind {
            ChangeKind::Add | ChangeKind::Change | ChangeKind::Unlink => self.ingest(store),
        }
    }

    /// Apply the conflict rule to whatever is on disk, then re-render.
    ///
    /// Call at a turn boundary, or from [`GoalProjection::ingest_event`]. The
    /// objective is adopted into SQL through [`GoalStore::update_objective`], so
    /// the oversized-objective spill and the 4,000-character cap apply to a hand
    /// edit exactly as they do to a tool call, and the next turn's injection
    /// (`crate::continuation::GoalContinuation::injection`) reads the adopted text
    /// because it reads SQL.
    ///
    /// # Errors
    ///
    /// [`GoalError::Db`] when the goal cannot be read or written, and
    /// [`GoalError::Document`] on an I/O failure.
    pub fn ingest(&self, store: &GoalStore) -> Result<Ingest, GoalError> {
        let Some(goal) = store.goal(&self.session_id)? else {
            return Ok(Ingest::NoGoal);
        };
        let Some(raw) = read_optional(&self.path)? else {
            self.write(&goal)?;
            return Ok(Ingest::Restored);
        };
        if self
            .lock()
            .as_ref()
            .is_some_and(|last| last.document == raw)
        {
            return Ok(Ingest::OwnRender);
        }
        let Some(document) = parse(&raw) else {
            let backup = back_up(&self.path, &raw)?;
            self.write_notes(
                &goal,
                &Notes {
                    rejected: Vec::new(),
                    salvaged: Some(backup.clone()),
                },
            )?;
            return Ok(Ingest::Salvaged { backup });
        };

        // The baseline is what *we* last rendered, not what SQL says now. SQL may
        // have moved on since the render — `updated_at_ms` changes on every write
        // — and comparing against it would report fields the user never touched.
        let baseline = self
            .lock()
            .as_ref()
            .map_or_else(|| goal.clone(), |last| last.goal.clone());

        let mut rejected = Vec::new();
        for field in Field::ALL {
            let Some(observed) = document.fields.get(field.key()) else {
                continue;
            };
            if *observed != field.value(&baseline) {
                rejected.push(RejectedEdit {
                    edited: Edited::Field(field),
                    attempted: observed.clone(),
                    actual: field.value(&goal),
                    refusal: field.owner(),
                });
            }
        }
        for check in Check::ALL {
            let Some(observed) = document.checks.get(check.key()) else {
                continue;
            };
            if *observed != check.state(&baseline) {
                rejected.push(RejectedEdit {
                    edited: Edited::Check(check),
                    attempted: box_of(*observed).to_owned(),
                    actual: check.rendered(&goal).to_owned(),
                    refusal: Refusal::SystemOwned {
                        noun: "the checklist",
                        plural: false,
                    },
                });
            }
        }

        let mut adopted = None;
        let mut goal = goal;
        if document.objective != baseline.objective {
            match store.update_objective(&self.session_id, &document.objective) {
                Ok(Some(updated)) => {
                    adopted = Some(updated.objective.clone());
                    goal = updated;
                }
                Ok(None) => return Ok(Ingest::NoGoal),
                Err(GoalError::EmptyObjective) => rejected.push(RejectedEdit {
                    edited: Edited::Objective,
                    attempted: document.objective.clone(),
                    actual: goal.objective.clone(),
                    refusal: Refusal::Unstorable {
                        reason: "a goal objective must not be empty".to_owned(),
                    },
                }),
                Err(other) => return Err(other),
            }
        }

        self.write_notes(
            &goal,
            &Notes {
                rejected: rejected.clone(),
                salvaged: None,
            },
        )?;
        Ok(Ingest::Applied { adopted, rejected })
    }

    fn lock(&self) -> MutexGuard<'_, Option<LastRender>> {
        self.last
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

const HEADER: &str = "\
# Goal

<!--
This document is a projection of Zuno's goal database.

The objective between the two `goal:objective` markers is yours to edit: the next
turn adopts whatever you leave there, spilling it to a file if it is longer than
the objective cap. Everything below it is read-only. The status, the budget, the
counters and the checklist are the system's to set, and an edit to one of them is
reported under `## Rejected edits` rather than applied -- a document that could
complete a goal would let an editor left open on a stale copy resurrect a
finished run by saving.

Nothing you write here is discarded quietly. If an edit is refused, this file
says which one and why.
-->
";

fn push_field(out: &mut String, field: Field, goal: &Goal) {
    out.push_str("- `");
    out.push_str(field.key());
    out.push_str("`: ");
    out.push_str(&field.value(goal));
    out.push('\n');
}

fn field_line(line: &str) -> Option<(Field, String)> {
    let (key, value) = line.strip_prefix("- `")?.split_once("`: ")?;
    let field = Field::ALL.into_iter().find(|field| field.key() == key)?;
    Some((field, value.trim().to_owned()))
}

fn check_line(line: &str) -> Option<(Check, bool)> {
    let (state, rest) = checkbox(line)?;
    let (key, _) = rest.strip_prefix('`')?.split_once('`')?;
    let check = Check::ALL.into_iter().find(|check| check.key() == key)?;
    Some((check, state))
}

fn checkbox(line: &str) -> Option<(bool, &str)> {
    for (marker, state) in [("- [x] ", true), ("- [ ] ", false)] {
        if let Some(rest) = line.strip_prefix(marker) {
            return Some((state, rest));
        }
    }
    None
}

fn box_of(state: bool) -> &'static str {
    if state { "[x]" } else { "[ ]" }
}

fn read_optional(path: &Path) -> Result<Option<String>, GoalError> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(Some(raw)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(GoalError::Document {
            operation: "read",
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Write `body` so a concurrent reader observes either all of it or none of it.
///
/// Temporary file in the destination's own directory, then rename: same
/// filesystem, so the rename is atomic and no lock is needed. The same technique
/// as `zuno-memory/src/store.rs`'s private helper, for the reasons recorded in this
/// module's docs — including why neither implementation calls `sync_all`.
///
/// The temporary name is built with `with_file_name` rather than `with_extension`
/// so it cannot collide with the target for any session id, and carries nanos so
/// two concurrent renders cannot collide with each other.
fn write_atomic(path: &Path, body: &str) -> Result<(), GoalError> {
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(|source| GoalError::Document {
        operation: "create directory",
        path: parent.to_path_buf(),
        source,
    })?;
    let name = path.file_name().map_or_else(
        || "goal.md".to_owned(),
        |name| name.to_string_lossy().into(),
    );
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let temporary = path.with_file_name(format!("{name}.tmp.{nanos}"));

    std::fs::write(&temporary, body).map_err(|source| GoalError::Document {
        operation: "write",
        path: temporary.clone(),
        source,
    })?;
    std::fs::rename(&temporary, path).map_err(|source| {
        drop(std::fs::remove_file(&temporary));
        GoalError::Document {
            operation: "rename",
            path: path.to_path_buf(),
            source,
        }
    })
}

/// Preserve bytes this crate is about to overwrite but could not understand.
///
/// Second resolution, matching `zuno-memory`'s `.bak.<ts>` snapshot: two salvages
/// inside one second write the same path, which is harmless because the second
/// one is preserving the same unparsable bytes as the first.
fn back_up(path: &Path, raw: &str) -> Result<PathBuf, GoalError> {
    let name = path.file_name().map_or_else(
        || "goal.md".to_owned(),
        |name| name.to_string_lossy().into(),
    );
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let backup = path.with_file_name(format!("{name}.bak.{seconds}"));
    std::fs::write(&backup, raw).map_err(|source| GoalError::Document {
        operation: "back up",
        path: backup.clone(),
        source,
    })?;
    Ok(backup)
}

#[cfg(test)]
#[path = "projection_tests.rs"]
mod tests;

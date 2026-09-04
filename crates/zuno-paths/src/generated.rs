//! The registry of paths Zuno generates inside a worktree, and the checks built on it.
//!
//! Zuno writes working state into the user's checkout: the goal projection, tool output
//! too large to return in full, the status of a background command. Each is a path some
//! crate creates under the project directory, and until this module existed the one
//! that was excluded from git was spelled as a literal in the crate that writes it and
//! again in the host that excludes it. Two spellings drift: a renamed directory keeps
//! the old pattern in the exclude file while the new files land in `git status`, where
//! a model reads them as the user's uncommitted work.
//!
//! Two lists decide every answer here. [`GENERATED_PATHS`] names each runtime path
//! with the git pattern that hides it, the reason it exists, and whether its entries
//! belong to a session. [`USER_OWNED_ENTRIES`] names each entry directly under the
//! project directory that a person authors and commits. Everything else is derived:
//! [`IGNORE_PATTERNS`] is what the host hands to [`crate::ensure_managed_block`],
//! [`is_generated`] answers whether a path is generated state, and
//! [`refuse_generated_state`] is the delivery check that keeps such a path out of a
//! commit.
//!
//! # Why the registry is a `const`
//!
//! A registry a caller can extend at runtime is a registry nobody can audit. Every
//! consumer of [`is_generated`] trusts it to say "not source" only about paths Zuno
//! itself produced; a plugin that appended `src/` at startup would make the delivery
//! check wave real source through and the exclude block hide it. A `const` can be read
//! in full in this file, and adding to it is a reviewed change that has to say what the
//! new path is, why it exists, and which code writes it.
//!
//! # Why the default under `.zuno/` is "generated"
//!
//! The registry used to be the whole answer: a path was generated only when it matched
//! a registered entry, and everything else under the project directory was source. That
//! is the wrong default, and it is how a session came to commit its own `.zuno/`. A
//! directory Zuno starts writing — this release, or the next one, or a subdirectory a
//! release renames — is not in the exclude block until somebody remembers to register
//! it, and until then `git add -A` collects it and the delivery check waves it through.
//! The failure is silent and it lands in the user's history.
//!
//! So the default is inverted. Anything under [`PROJECT_DIRECTORY`] that is not one of
//! [`USER_OWNED_ENTRIES`] is generated state, reported as
//! [`GeneratedReason::UnregisteredProjectState`]. The registry stays, because a
//! registered path can name *why* it exists in a report; what it no longer does is
//! decide whether an unnamed path is safe to commit.
//!
//! [`USER_OWNED_ENTRIES`] is therefore the thing to keep honest: it is every entry a
//! person authors — `zuno.json`, `skill/`, `agent/`, `command/`, `extensions/`,
//! `plans/`, `rules/`, `RULES.md` — and each name there is either a name Zuno's own
//! loaders read from the project directory or one Zuno's own documentation tells a
//! person to author there. Plans are among them because the model writes them through
//! the ordinary `write` tool under the user's permission rules, and `zuno-agent`'s
//! `plan_file` documents them as a path the user may gitignore or commit as they choose.
//! `rules/` is among them because `docs/config/instructions.md` publishes
//! `.zuno/rules/*.md` as the example `instructions` glob: a released page that tells
//! people to write source there is exactly as binding as a loader that reads it, and
//! default deny without that entry would hide, refuse and drop from checkpoints a file
//! Zuno asked them to write.
//!
//! # Why an ambiguous spelling is resolved in the user's favour
//!
//! Two comparisons could go either way, and both are decided the same direction: a
//! wrongly refused commit blocks a person's own work, while a missed one is still
//! caught by the `.gitignore` that every generated directory carries
//! ([`crate::generated_dir`]). So the project directory's own name is compared exactly
//! — a `.ZUNO/` on a case-sensitive filesystem is somebody else's directory — and a
//! user-owned entry's name is compared without ASCII case, because git folds case
//! itself wherever `core.ignorecase` is set, which is every Windows and macOS
//! checkout. `.zuno/Zuno.json` is re-included by git there, and this module must not
//! then call it generated.
//!
//! # Why the check can be lexical
//!
//! [`is_generated`] is a component-wise comparison rather than a gitignore matcher,
//! and stays honest only because both shapes it has to agree with are constrained at
//! compile time. A registered pattern is a plain directory prefix — `.zuno/<name>/`,
//! slash separated, no glob characters, a trailing slash so it names a directory and
//! not a file that shares the name. A rendered pattern in [`IGNORE_PATTERNS`] is
//! either [`PROJECT_STATE_PATTERN`] or one negated user-owned entry, each carrying
//! git's [`ANY_DEPTH`] prefix so that it reads at every depth exactly as [`classify`]
//! does, and nothing else: that is the only glob shape git and this module read the
//! same way.
//!
//! ```
//! use std::path::Path;
//! use zuno_paths::generated::{is_generated, refuse_generated_state};
//!
//! let worktree = Path::new("/repo");
//! assert!(is_generated(worktree, Path::new(".zuno/goal/ses_1.md")));
//! assert!(is_generated(worktree, Path::new("/repo/src/../.zuno/tool-output/tool_x")));
//! // Not registered, not authored by a person: generated all the same.
//! assert!(is_generated(worktree, Path::new(".zuno/whatever-comes-next/state.json")));
//! // A project directory in a subdirectory is read the same way, at either end.
//! assert!(is_generated(worktree, Path::new("crates/foo/.zuno/tool-output/tool_x")));
//! assert!(!is_generated(worktree, Path::new("crates/foo/.zuno/zuno.json")));
//! assert!(!is_generated(worktree, Path::new(".zuno/zuno.json")));
//! assert!(!is_generated(worktree, Path::new(".zuno/rules/house.md")));
//! assert!(!is_generated(worktree, Path::new(".zuno/plans/1700000000000-swift-otter.md")));
//! assert!(!is_generated(worktree, Path::new("/elsewhere/.zuno/goal/ses_1.md")));
//!
//! let refusal = refuse_generated_state(worktree, [".zuno/goal/ses_1.md", "src/lib.rs"])
//!     .expect_err("a staged goal document is generated state");
//! assert_eq!(refusal.offending.len(), 1);
//! assert!(refusal.report().contains("goal projection"));
//! ```

use std::ffi::OsStr;
use std::fmt;
use std::path::{Component, Path, PathBuf, Prefix};

use crate::config_chain::PROJECT_DIRECTORY;

/// One runtime path Zuno creates inside a worktree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GeneratedPath {
    /// The directory Zuno creates, spelled the way a git pattern spells one:
    /// `.zuno/<name>/`, slash-separated, with a trailing slash, and relative to
    /// whichever directory holds the project directory.
    ///
    /// It names the directory in a report and composes the native path
    /// [`crate::generated_dir::GeneratedDirectory`] creates; what the exclude block
    /// contains is rendered from [`IGNORE_PATTERNS`] instead, because one pattern per
    /// registered directory is what let an unregistered one through.
    ///
    /// Slash-separated on every platform because a git pattern is. Composing it from
    /// [`PROJECT_DIRECTORY`] with `PathBuf::join` would produce a backslash on Windows
    /// and silently stop matching, which is why each pattern is spelled out and a test
    /// asserts it agrees with the directory names the writers use.
    pub pattern: &'static str,
    /// Why the path exists, as one sentence a human can read in a report.
    pub reason: &'static str,
    /// Whether every entry beneath the path is named for the session that produced it.
    ///
    /// A per-session path goes stale when its session ends, so a cleanup can be keyed by
    /// session id. A path that is not per-session belongs to the workspace or the
    /// process and is reconciled by the service that writes it.
    pub per_session: bool,
}

impl GeneratedPath {
    /// One line for a human-facing report: the pattern, the reason, the scope.
    #[must_use]
    pub fn describe(&self) -> String {
        let scope = if self.per_session {
            "one entry per session"
        } else {
            "workspace-wide"
        };
        format!("`{}` — {} ({scope})", self.pattern, self.reason)
    }

    /// The pattern's directory names, outermost first.
    ///
    /// How [`crate::generated_dir::GeneratedDirectory`] composes the native path from
    /// the git pattern: the pattern is the single spelling, and joining its own
    /// segments is what keeps the directory that gets created and the pattern that
    /// hides it from ever being two different places.
    pub(crate) fn segments(&self) -> impl Iterator<Item = &'static str> {
        self.pattern
            .split('/')
            .filter(|segment| !segment.is_empty())
    }

    /// Whether `relative`, a worktree-relative path as ordinary names, is this
    /// directory or lies beneath it.
    fn covers(&self, relative: &[&OsStr]) -> bool {
        let mut names = relative.iter();
        self.segments().all(|segment| {
            names
                .next()
                .is_some_and(|name| *name == OsStr::new(segment))
        })
    }
}

/// `.zuno/goal/<sessionID>.md`, the human-editable rendering of the goal database.
///
/// Written by `GoalProjection` in `crates/zuno-goal/src/projection.rs` (`document_path`
/// chooses the directory), together with the `<name>.bak.<seconds>` copy it keeps of a
/// document it could not parse.
pub const GOAL_PROJECTION: GeneratedPath = GeneratedPath {
    pattern: ".zuno/goal/",
    reason: "the goal projection, a Markdown rendering of the session's goal database \
             that is rewritten on every material change",
    per_session: true,
};

/// `.zuno/tool-output/tool_<sessionID>_<uuid>`, tool output too large to return in full.
///
/// Written by `ToolOutputStore::persist` in `crates/zuno-tool/src/store.rs`; rooted
/// under the worktree by `ShellTool::with_sandbox_backend` in
/// `crates/zuno-tools/src/shell.rs` and `ToolRegistryBuilder::build` in
/// `crates/zuno-tools/src/registry.rs`.
pub const TOOL_OUTPUT: GeneratedPath = GeneratedPath {
    pattern: ".zuno/tool-output/",
    reason: "tool output too large to hand back to the model in full, persisted so it \
             can be read back in pieces",
    per_session: true,
};

/// `.zuno/background/<id>.status.json`, the `<id>.status.json.tmp` it is staged through,
/// `<id>.output`, and `<id>.lock`: the four names one of the shell tool's background
/// commands owns.
///
/// Written by `BackgroundExecutionService` in `crates/zuno-pty/src/background.rs`;
/// rooted under the worktree by `ShellTool::with_sandbox_backend` in
/// `crates/zuno-tools/src/shell.rs` and by the CLI's process-wide service cache in
/// `crates/zuno-cli/src/environment.rs`.
pub const BACKGROUND_EXECUTIONS: GeneratedPath = GeneratedPath {
    pattern: ".zuno/background/",
    reason: "the status and captured output of background shell commands, kept so a \
             restart can tell a finished command from an interrupted one",
    per_session: false,
};

/// Every runtime path Zuno creates inside a worktree.
///
/// A `const` rather than anything a caller can extend; see the module documentation
/// for why. Adding an entry means also proving, in its doc comment, which code writes
/// the path.
///
/// This list names paths; it does not decide what is generated. A path under the
/// project directory that is in no entry here is still generated state — reported as
/// [`GeneratedReason::UnregisteredProjectState`] — so forgetting to register a new
/// directory costs a less specific report and never a committed one.
pub const GENERATED_PATHS: &[GeneratedPath] =
    &[GOAL_PROJECTION, TOOL_OUTPUT, BACKGROUND_EXECUTIONS];

/// The pattern prefix that makes git read a pattern at every depth in the worktree.
///
/// A pattern with a `/` in the middle is anchored where the ignore file lives, which
/// for `info/exclude` is the worktree root, so `.zuno/*` alone says nothing about
/// `sub/.zuno/`. A leading `**/` is git's own spelling for "in any directory", and it
/// still matches at the root, so one rendered set covers both depths.
///
/// Public because [`crate::exclude`] has to recognise a block line written before the
/// patterns carried the prefix: the prefix says where git looks, not who wrote the line.
pub const ANY_DEPTH: &str = "**/";

/// The git pattern that excludes everything directly under a project directory.
///
/// `**/.zuno/*`, and never `**/.zuno/`. Git does not descend into a directory an
/// ignore rule already excluded, so a directory pattern would make every negation
/// below it unreachable: a user's `zuno.json` could not be brought back, and the
/// pattern set would hide the configuration they are trying to commit. `**/.zuno/*`
/// excludes a project directory's direct children, which git still enumerates, so one
/// `!` per user-owned entry re-includes it — and because `*` does not cross a `/`,
/// nothing below a re-included directory is excluded either.
///
/// The `**/` is what covers a project directory in a subdirectory: the configuration
/// chain reads one there, and a release that rooted generated state at the session's
/// own directory wrote one there. [`classify`] reads a project directory at any depth
/// for the same two reasons, so the pattern and the classifier still answer alike.
pub const PROJECT_STATE_PATTERN: &str = "**/.zuno/*";

/// Declare the entries a person owns, and render the exclude patterns from them.
///
/// One list, two consts, so an entry cannot be taught to the classifier and forgotten
/// in the exclude block. [`concat!`] needs literals, which is why the project
/// directory and [`ANY_DEPTH`] are spelled here rather than composed from their
/// consts; the compile-time assertion below rejects every rendered pattern that is not
/// `**/` followed by that project directory, so the three cannot drift.
macro_rules! user_owned_entries {
    ($($entry:literal),+ $(,)?) => {
        /// Every entry directly under the project directory that a person authors.
        ///
        /// The complement of this list is generated state — see the module
        /// documentation for why that is the default — so each name here has to be a
        /// name one of Zuno's own loaders reads from `<worktree>/.zuno/`, or one
        /// Zuno's own published documentation tells a person to author there:
        ///
        /// | Entry | Read by |
        /// | --- | --- |
        /// | `zuno.json`, `zuno.jsonc` | the configuration chain, through [`crate::config_chain::CONFIG_FILE_STEM`] |
        /// | `tui.json`, `tui.jsonc` | `zuno-cli`'s `tui_config_paths` |
        /// | `RULES.md` | `zuno-memory`'s `Scope::Project` |
        /// | `skill`, `skills` | `zuno-catalog`'s `skill::scan` prefixes |
        /// | `agent`, `agents` | `zuno-catalog`'s `AGENT_DIRECTORY_PREFIXES` |
        /// | `command`, `commands` | `zuno-catalog`'s `COMMAND_DIRECTORY_PREFIXES` |
        /// | `extensions` | `zuno-extension`'s `STATIC_DIRECTORY` |
        /// | `plans` | `zuno-agent`'s `PLANS_DIRECTORY`, written by the model through the `write` tool |
        /// | `rules` | the `instructions` array, whose published example is `.zuno/rules/*.md` |
        ///
        /// Compared without ASCII case; see the module documentation for why an
        /// ambiguous spelling is resolved in the user's favour.
        pub const USER_OWNED_ENTRIES: &[&str] = &[$($entry),+];

        /// The exclude patterns for [`crate::ensure_managed_block`], in the order git
        /// has to read them.
        ///
        /// [`PROJECT_STATE_PATTERN`] first, then one `!**/.zuno/<entry>` per
        /// [`USER_OWNED_ENTRIES`] entry: a later pattern wins in git, so the
        /// negations have to follow the exclusion they undo. Rendered from the list
        /// the classifier reads, so the block and [`is_generated`] cannot disagree
        /// about which entries belong to the user.
        ///
        /// A registered pattern is deliberately *not* here. `.zuno/*` already covers
        /// every registered directory, and naming them again would say that the ones
        /// nobody registered are somebody's to commit.
        pub const IGNORE_PATTERNS: &[&str] = &[
            PROJECT_STATE_PATTERN,
            $(concat!("!", "**/", ".zuno/", $entry)),+
        ];
    };
}

user_owned_entries!(
    "RULES.md",
    "agent",
    "agents",
    "command",
    "commands",
    "extensions",
    "plans",
    "rules",
    "skill",
    "skills",
    "tui.json",
    "tui.jsonc",
    "zuno.json",
    "zuno.jsonc",
);

/// Whether `entry`, one name directly under the project directory, is user-owned.
///
/// ASCII case is folded because git folds it wherever `core.ignorecase` is set, which
/// is every Windows and macOS checkout: `.zuno/Zuno.json` is re-included there by
/// `!.zuno/zuno.json`, and this module must not then call it generated. The bytes are
/// compared rather than the `str`, so a name that is not UTF-8 is an ordinary
/// mismatch instead of an unanswerable question.
fn is_user_owned(entry: &OsStr) -> bool {
    let entry = entry.as_encoded_bytes();
    USER_OWNED_ENTRIES
        .iter()
        .any(|owned| entry.eq_ignore_ascii_case(owned.as_bytes()))
}

/// Whether `pattern` is `.zuno/<directory>[/<directory>…]/` and nothing more.
///
/// The shape [`is_generated`] relies on: anchored under [`PROJECT_DIRECTORY`], slash
/// separated, a trailing slash, at least one directory below the project directory, no
/// empty or dot segments, and none of the characters git reads as a glob, an escape, a
/// negation, a comment or a line break. Anything else would be a pattern git and the
/// lexical check read differently.
const fn is_plain_directory_pattern(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    let project = PROJECT_DIRECTORY.as_bytes();
    // `.zuno/` plus at least one character and the closing slash.
    if bytes.len() < project.len() + 3 {
        return false;
    }
    let mut index = 0;
    while index < project.len() {
        if bytes[index] != project[index] {
            return false;
        }
        index += 1;
    }
    if bytes[project.len()] != b'/' || bytes[bytes.len() - 1] != b'/' {
        return false;
    }
    let mut start = project.len() + 1;
    let mut segments = 0;
    let mut cursor = start;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if byte == b'/' {
            let length = cursor - start;
            if length == 0
                || (length == 1 && bytes[start] == b'.')
                || (length == 2 && bytes[start] == b'.' && bytes[start + 1] == b'.')
            {
                return false;
            }
            segments += 1;
            start = cursor + 1;
        } else if byte < b' ' || matches!(byte, b'*' | b'?' | b'[' | b']' | b'!' | b'\\' | b'#') {
            return false;
        }
        cursor += 1;
    }
    segments >= 1
}

const _: () = {
    let mut index = 0;
    while index < GENERATED_PATHS.len() {
        assert!(
            is_plain_directory_pattern(GENERATED_PATHS[index].pattern),
            "a registered pattern must be `.zuno/<directory>/`: slash-separated, no glob \
             characters, a trailing slash, and never the project directory itself"
        );
        index += 1;
    }
};

/// Whether `bytes` contains `needle` starting at `offset`.
///
/// Spelled out because a `const fn` cannot slice and compare; every rendered-pattern
/// check is a prefix comparison against a const, so one helper covers all of them.
const fn matches_at(bytes: &[u8], offset: usize, needle: &[u8]) -> bool {
    if bytes.len() < offset + needle.len() {
        return false;
    }
    let mut index = 0;
    while index < needle.len() {
        if bytes[offset + index] != needle[index] {
            return false;
        }
        index += 1;
    }
    true
}

/// Whether `pattern` is one of the two shapes [`IGNORE_PATTERNS`] may contain.
///
/// Either [`PROJECT_STATE_PATTERN`] — [`ANY_DEPTH`], the project directory, a slash,
/// and a lone `*` — or `!` followed by the same prefix and one entry name that is
/// itself glob-free. Nothing else: no second `*`, no `?`, no character class, no
/// nested path, no leading slash, no trailing slash, and no whitespace, which git
/// strips from the end of a pattern unless it is escaped. Those two shapes are the
/// ones git and [`classify`] read the same way, and rendering anything else would
/// give the exclude block a reach the lexical check cannot reproduce.
const fn is_rendered_pattern(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    let project = PROJECT_DIRECTORY.as_bytes();
    let negated = !bytes.is_empty() && bytes[0] == b'!';
    let start = if negated { 1 } else { 0 };
    if !matches_at(bytes, start, ANY_DEPTH.as_bytes()) {
        return false;
    }
    let start = start + ANY_DEPTH.len();
    if !matches_at(bytes, start, project) {
        return false;
    }
    // The separator after the project directory, plus at least one character.
    let name = start + project.len() + 1;
    if bytes.len() < name + 1 || bytes[name - 1] != b'/' {
        return false;
    }
    if !negated {
        return bytes.len() == name + 1 && bytes[name] == b'*';
    }
    let mut cursor = name;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if byte <= b' ' || matches!(byte, b'*' | b'?' | b'[' | b']' | b'!' | b'\\' | b'#' | b'/') {
            return false;
        }
        cursor += 1;
    }
    let length = cursor - name;
    !(length == 1 && bytes[name] == b'.'
        || length == 2 && bytes[name] == b'.' && bytes[name + 1] == b'.')
}

/// Whether a rendered pattern re-includes rather than excludes.
const fn is_negated(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    !bytes.is_empty() && bytes[0] == b'!'
}

const _: () = {
    assert!(
        !is_negated(IGNORE_PATTERNS[0]) && is_rendered_pattern(IGNORE_PATTERNS[0]),
        "the first rendered pattern must be the project-state exclusion: git reads a \
         later pattern as the winner, so a negation before it would be undone"
    );
    let mut index = 1;
    while index < IGNORE_PATTERNS.len() {
        assert!(
            is_negated(IGNORE_PATTERNS[index]) && is_rendered_pattern(IGNORE_PATTERNS[index]),
            "every pattern after the project-state exclusion must re-include one \
             user-owned entry directly under the project directory"
        );
        index += 1;
    }
};

/// Why a path is generated state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GeneratedReason {
    /// A path under one of the registered runtime directories, which names what it is
    /// and which code writes it.
    Registered(&'static GeneratedPath),
    /// Something under the project directory that is neither registered nor one of
    /// [`USER_OWNED_ENTRIES`].
    ///
    /// The honest answer for a directory a release started writing without registering
    /// it, and the reason a path like that is kept out of a commit instead of being
    /// waved through until somebody notices.
    UnregisteredProjectState,
}

impl GeneratedReason {
    /// The git pattern that hides the path.
    #[must_use]
    pub fn pattern(&self) -> &'static str {
        match self {
            Self::Registered(generated) => generated.pattern,
            Self::UnregisteredProjectState => PROJECT_STATE_PATTERN,
        }
    }

    /// Why the path exists, as one clause a human can read in a report.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Registered(generated) => generated.reason,
            Self::UnregisteredProjectState => UNREGISTERED_REASON,
        }
    }

    /// The registry entry, when the path is under a registered directory.
    #[must_use]
    pub fn registered(&self) -> Option<&'static GeneratedPath> {
        match self {
            Self::Registered(generated) => Some(generated),
            Self::UnregisteredProjectState => None,
        }
    }
}

/// What [`GeneratedReason::UnregisteredProjectState`] reads as in a report.
const UNREGISTERED_REASON: &str = "state under Zuno's project directory that is not one of \
     the entries a person authors, so it is the runtime's and not the repository's";

/// Why `path` is generated state, or `None` when it is not.
///
/// Under a project directory the answer is "generated" unless the entry it sits in is
/// one of [`USER_OWNED_ENTRIES`]; outside one, nothing is generated. A registered
/// directory is reported as [`GeneratedReason::Registered`] so a caller can name the
/// reason it exists, and everything else under `.zuno/` as
/// [`GeneratedReason::UnregisteredProjectState`]. The project directory itself is not
/// generated state: it holds the user's own configuration.
///
/// A project directory at any depth counts, not only the one at the worktree root. Two
/// facts make that the right reading, and each of them is a path that exists in real
/// checkouts. The configuration chain walks `.zuno` up from the session's directory to
/// the worktree root ([`crate::config_chain`]), so `sub/.zuno/zuno.json` is the user's
/// configuration and `sub/.zuno/skill/` their skills — resolved by the same
/// [`USER_OWNED_ENTRIES`] list, at whatever depth they sit. And releases that rooted
/// generated state at the session's own directory left `sub/.zuno/tool-output/` and
/// `sub/.zuno/background/` on disk in checkouts that are still in use; nothing rewrites
/// or deletes them, so the classifier and the rendered patterns are what keep them out
/// of a commit now that the writers root at the worktree.
///
/// `path` is taken as absolute or as relative to `worktree`, and `.` and `..` are
/// resolved lexically, without touching the filesystem — so a path git printed, a path
/// a tool joined, and a path a user typed all get the same answer and none of them has
/// to exist yet. A path outside the worktree is not generated state, and neither is
/// one that cannot be related to it: an absolute path when the worktree is itself
/// relative, or a drive-relative Windows path. Nothing here panics on such input;
/// "outside" is an ordinary answer.
///
/// Comparison is component-wise in the platform's own separators, so a Windows path
/// spelled with backslashes matches the slash-separated pattern, while on Unix a
/// backslash is an ordinary filename byte and is not a separator, which is how git
/// reads it too. The project directory's own name is compared exactly and a user-owned
/// entry's without ASCII case; the module documentation says why each side is decided
/// that way.
#[must_use]
pub fn classify(worktree: &Path, path: &Path) -> Option<GeneratedReason> {
    let relative = relative_to_worktree(worktree, path)?;
    let project = relative
        .iter()
        .position(|name| *name == OsStr::new(PROJECT_DIRECTORY))?;
    let inside = &relative[project..];
    let [_, entry, ..] = inside else {
        return None;
    };
    if let Some(generated) = GENERATED_PATHS
        .iter()
        .find(|generated| generated.covers(inside))
    {
        return Some(GeneratedReason::Registered(generated));
    }
    if is_user_owned(entry) {
        return None;
    }
    Some(GeneratedReason::UnregisteredProjectState)
}

/// Whether `path`, inside `worktree`, is generated working state rather than source.
///
/// See [`classify`] for exactly what is accepted and how it is compared.
#[must_use]
pub fn is_generated(worktree: &Path, path: &Path) -> bool {
    classify(worktree, path).is_some()
}

/// A staged or changed path that turned out to be generated state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedGeneratedPath {
    /// The path exactly as the caller reported it.
    pub path: PathBuf,
    /// Why it is generated state, which names the reason it exists.
    pub generated: GeneratedReason,
}

/// The refusal [`refuse_generated_state`] returns: generated state is about to be
/// committed.
///
/// Carries every offending path with the registry entry behind it, so a caller can
/// report each one with the reason it exists rather than a bare filename. [`Display`]
/// is the one-line form for a log; [`GeneratedStateStaged::report`] is the one to show
/// a person, because it includes the remedy.
///
/// [`Display`]: fmt::Display
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedStateStaged {
    /// The generated paths among those reported, in the order they were reported.
    pub offending: Vec<StagedGeneratedPath>,
}

impl GeneratedStateStaged {
    /// What to do about it, and why it happened.
    ///
    /// The exclude block and the directory's own `.gitignore` normally keep these
    /// paths out of the index altogether, so a generated path reaching a commit has a
    /// short list of causes, and the remedy names them: both exclusions were removed,
    /// the file was added with `--force`, or git already tracks it. The last is the one
    /// no exclusion can answer — an ignore rule never applies to a tracked path — so the
    /// remedy has to name `git rm --cached` as well as `git restore --staged`, or a user
    /// whose earlier release committed a goal document is told to do something that
    /// leaves the next `git commit -a` delivering it again.
    pub const REMEDY: &'static str = "Unstage each one (`git restore --staged -- <path>`) \
        and leave it out of the commit. The repository-private exclude block and the \
        directory's own `.gitignore` should already have hidden it, so it is here because \
        both were removed, the file was force-added, or git already tracks it — which no \
        ignore rule undoes, so `git rm --cached -- <path>` is what stops it being \
        delivered again.";

    /// What to do about a path under the project directory that Zuno does not
    /// recognise as anybody's source.
    ///
    /// Everything under `.zuno/` other than the entries a person authors belongs to the
    /// runtime, so the remedy has to say where a file of one's own goes instead —
    /// otherwise the refusal reads as a bug in the check rather than as an answer.
    pub const UNREGISTERED_REMEDY: &'static str = "Everything directly under `.zuno/` \
        other than the entries a person authors — `zuno.json`, `tui.json`, `RULES.md`, \
        `skill/`, `agent/`, `command/`, `plans/`, `rules/`, `extensions/` — is Zuno's own \
        working state and is not committed. Unstage each one (`git restore --staged -- \
        <path>`); if one of them is a file you wrote and want in the repository, keep it \
        outside `.zuno/`.";

    /// The human-facing report: one line per path with its reason, then the remedy for
    /// each kind of path that is in it.
    #[must_use]
    pub fn report(&self) -> String {
        let mut report = String::new();
        let count = self.offending.len();
        let verb = if count == 1 { "path is" } else { "paths are" };
        report.push_str(&format!(
            "{count} staged {verb} Zuno's generated working state, not source:\n"
        ));
        for staged in &self.offending {
            report.push_str(&format!(
                "  {} — {}\n",
                crate::display_path(&staged.path),
                staged.generated.reason()
            ));
        }
        let mut remedies = Vec::new();
        if self
            .offending
            .iter()
            .any(|staged| staged.generated.registered().is_some())
        {
            remedies.push(Self::REMEDY);
        }
        if self
            .offending
            .iter()
            .any(|staged| staged.generated == GeneratedReason::UnregisteredProjectState)
        {
            remedies.push(Self::UNREGISTERED_REMEDY);
        }
        for remedy in remedies {
            report.push_str(remedy);
            report.push('\n');
        }
        report
    }
}

impl fmt::Display for GeneratedStateStaged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("refusing to commit generated state: ")?;
        for (index, staged) in self.offending.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(
                f,
                "{} (matches `{}`)",
                crate::display_path(&staged.path),
                staged.generated.pattern()
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for GeneratedStateStaged {}

/// The delivery check: refuse when any of `paths` is generated state.
///
/// `paths` is whatever git reported as staged or changed — `git diff --cached
/// --name-only -z`, or the paths from `git status --porcelain -z` — and this function
/// deliberately does not run git itself. Taking the list as input keeps the check pure,
/// so it is testable without a repository and callable from whichever step already
/// holds the list, and it cannot disagree with the caller about which repository or
/// index was inspected. Use git's `-z` form: a quoted porcelain path is not a path.
///
/// Every reported path is classified, so a refusal names all of them at once rather
/// than one per attempt.
///
/// # Errors
///
/// [`GeneratedStateStaged`] listing every generated path in `paths`, in the order
/// given. `Ok(())` when none is generated, including for an empty list.
pub fn refuse_generated_state<I, P>(worktree: &Path, paths: I) -> Result<(), GeneratedStateStaged>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    let offending: Vec<StagedGeneratedPath> = paths
        .into_iter()
        .filter_map(|path| {
            let path = path.as_ref();
            classify(worktree, path).map(|generated| StagedGeneratedPath {
                path: path.to_path_buf(),
                generated,
            })
        })
        .collect();
    if offending.is_empty() {
        Ok(())
    } else {
        Err(GeneratedStateStaged { offending })
    }
}

/// A path reduced to what anchors it and the ordinary names under that anchor, with
/// `.` and `..` already applied.
struct Lexical<'a> {
    /// The Windows volume, when the path names one.
    prefix: Option<Prefix<'a>>,
    /// Whether the path starts at a root directory.
    rooted: bool,
    /// The remaining ordinary names, outermost first.
    names: Vec<&'a OsStr>,
}

impl Lexical<'_> {
    /// Whether the path is fixed to a place, rather than relative to an unknown one.
    fn anchored(&self) -> bool {
        self.rooted || self.prefix.is_some()
    }
}

/// Resolve `.` and `..` without consulting the filesystem.
///
/// `None` when a relative path climbs above where it started, because such a path names
/// something outside whatever it is relative to. `..` at the root of an absolute path
/// stays at the root, which is how every platform resolves it. Symbolic links are not
/// followed: the answer is about the path as spelled, which is also the only thing a
/// git exclude pattern is matched against.
fn lexical(path: &Path) -> Option<Lexical<'_>> {
    let mut lexical = Lexical {
        prefix: None,
        rooted: false,
        names: Vec::new(),
    };
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => lexical.prefix = Some(prefix.kind()),
            Component::RootDir => lexical.rooted = true,
            Component::CurDir => {}
            Component::ParentDir => {
                if lexical.names.pop().is_none() && !lexical.rooted {
                    return None;
                }
            }
            Component::Normal(name) => lexical.names.push(name),
        }
    }
    Some(lexical)
}

/// `path` as ordinary names below `worktree`, or `None` when it is not inside it.
///
/// A relative `path` is relative to the worktree by definition and needs no knowledge
/// of where the worktree is. An anchored one has to be anchored the same way as the
/// worktree — same volume, both rooted — and start with the worktree's own names; a
/// relative worktree cannot anchor an absolute path without asking the filesystem, so
/// that pairing is "outside" rather than a guess.
fn relative_to_worktree<'a>(worktree: &Path, path: &'a Path) -> Option<Vec<&'a OsStr>> {
    let path = lexical(path)?;
    if !path.anchored() {
        return Some(path.names);
    }
    let worktree = lexical(worktree)?;
    if !worktree.anchored()
        || worktree.rooted != path.rooted
        || !same_prefix(worktree.prefix, path.prefix)
    {
        return None;
    }
    // Spelled out rather than `strip_prefix`, whose pattern type would tie the
    // worktree's borrow to the path's and force a caller to keep both alive together.
    let (names, prefix) = (&path.names, &worktree.names);
    if names.len() < prefix.len()
        || names
            .iter()
            .zip(prefix)
            .any(|(name, expected)| name != expected)
    {
        return None;
    }
    Some(names[prefix.len()..].to_vec())
}

/// Whether two Windows path prefixes name the same volume.
///
/// `std::fs::canonicalize` returns a verbatim `\\?\C:\` prefix while a path git or a
/// user supplies carries a plain `C:\`, and a drive letter arrives in either case.
/// Comparing the components byte for byte would call a generated file foreign because
/// its worktree had been canonicalized, so the equivalent spellings are folded together.
/// On Unix neither side ever has a prefix and this is trivially true.
fn same_prefix(worktree: Option<Prefix<'_>>, path: Option<Prefix<'_>>) -> bool {
    match (worktree, path) {
        (None, None) => true,
        (Some(worktree), Some(path)) => match (volume(worktree), volume(path)) {
            (Some(worktree), Some(path)) => worktree.same_as(&path),
            _ => worktree == path,
        },
        _ => false,
    }
}

/// A Windows volume with its verbatim and plain spellings folded together.
enum Volume<'a> {
    Disk(u8),
    Share(&'a OsStr, &'a OsStr),
}

impl Volume<'_> {
    fn same_as(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Disk(left), Self::Disk(right)) => left.eq_ignore_ascii_case(right),
            (Self::Share(left_server, left_share), Self::Share(right_server, right_share)) => {
                left_server.eq_ignore_ascii_case(right_server)
                    && left_share.eq_ignore_ascii_case(right_share)
            }
            _ => false,
        }
    }
}

fn volume(prefix: Prefix<'_>) -> Option<Volume<'_>> {
    match prefix {
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => Some(Volume::Disk(letter)),
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
            Some(Volume::Share(server, share))
        }
        Prefix::Verbatim(_) | Prefix::DeviceNS(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config_chain::CONFIG_FILE_STEM, ensure_managed_block, files::BACKGROUND_DIRECTORY,
        files::TOOL_OUTPUT_DIRECTORY,
    };
    use std::fs;
    use std::process::Command;

    fn run(cwd: &Path, args: &[&str]) -> String {
        let null_device = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let output = Command::new(args[0])
            .args(&args[1..])
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", null_device)
            .env("GIT_CONFIG_SYSTEM", null_device)
            .env("GIT_AUTHOR_NAME", "zuno-paths")
            .env("GIT_AUTHOR_EMAIL", "zuno-paths@example.test")
            .env("GIT_COMMITTER_NAME", "zuno-paths")
            .env("GIT_COMMITTER_EMAIL", "zuno-paths@example.test")
            .output()
            .unwrap_or_else(|error| panic!("spawn {args:?}: {error}"));
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("git output is UTF-8")
    }

    fn repository() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path();
        run(path, &["git", "init", "--initial-branch=main", "."]);
        fs::write(path.join("file.txt"), "hello\n").expect("write file");
        run(path, &["git", "add", "file.txt"]);
        run(path, &["git", "commit", "-m", "initial"]);
        root
    }

    fn worktree() -> &'static Path {
        Path::new("/repo")
    }

    /// The registry is the audit surface, so its exact contents are asserted: adding
    /// or removing an entry has to change this test, on purpose.
    #[test]
    fn the_registry_lists_the_goal_projection_tool_output_and_background_state_and_nothing_else() {
        let patterns: Vec<&str> = GENERATED_PATHS
            .iter()
            .map(|generated| generated.pattern)
            .collect();
        assert_eq!(
            patterns,
            [".zuno/goal/", ".zuno/tool-output/", ".zuno/background/"]
        );
        let per_session: Vec<bool> = GENERATED_PATHS
            .iter()
            .map(|generated| generated.per_session)
            .collect();
        assert_eq!(
            per_session,
            [true, true, false],
            "goal documents and tool output are named for their session; background \
             state is named for its execution"
        );
        for generated in GENERATED_PATHS {
            assert!(
                !generated.reason.trim().is_empty() && !generated.reason.ends_with('.'),
                "the reason is a clause for a report, not a paragraph: {:?}",
                generated.reason
            );
        }
    }

    /// The patterns are spelled as literals because a git pattern is slash-separated
    /// on every platform; this is what keeps them and the writers' directory names
    /// from drifting apart.
    #[test]
    fn every_pattern_spells_its_directory_the_way_the_writer_does() {
        assert_eq!(
            TOOL_OUTPUT.pattern,
            format!("{PROJECT_DIRECTORY}/{TOOL_OUTPUT_DIRECTORY}/")
        );
        assert_eq!(
            BACKGROUND_EXECUTIONS.pattern,
            format!("{PROJECT_DIRECTORY}/{BACKGROUND_DIRECTORY}/")
        );
        assert_eq!(
            GOAL_PROJECTION.pattern,
            format!("{PROJECT_DIRECTORY}/goal/")
        );
    }

    /// The exclude block used to be one pattern per registered directory, which let
    /// `git add -A` pick up any directory nobody had registered. It is now the
    /// project-state exclusion followed by one re-inclusion per user-owned entry, and
    /// the order is what makes it work: git takes the last matching pattern, so a
    /// negation placed before the exclusion it undoes would do nothing.
    #[test]
    fn the_exclude_patterns_are_the_project_state_exclusion_then_one_negation_per_user_entry() {
        let mut expected = vec![PROJECT_STATE_PATTERN.to_owned()];
        expected.extend(
            USER_OWNED_ENTRIES
                .iter()
                .map(|entry| format!("!{ANY_DEPTH}{PROJECT_DIRECTORY}/{entry}")),
        );
        assert_eq!(IGNORE_PATTERNS, expected.as_slice());
        assert!(
            !IGNORE_PATTERNS.iter().any(|pattern| GENERATED_PATHS
                .iter()
                .any(|generated| generated.pattern == *pattern)),
            "naming the registered directories again would say the unregistered ones \
             are somebody's to commit"
        );
        for pattern in IGNORE_PATTERNS {
            assert!(
                !pattern.contains(['\n', '\r']),
                "`ensure_managed_block` refuses a multi-line entry: {pattern:?}"
            );
        }
    }

    /// `**/.zuno/*` and never `**/.zuno/`: git does not descend into a directory an
    /// ignore rule already excluded, so a directory pattern would make every negation
    /// below it unreachable and hide the user's own `zuno.json` from their commit. The
    /// `**/` is what reaches a project directory in a subdirectory, which the
    /// configuration chain reads and an older release wrote generated state into.
    #[test]
    fn the_project_state_pattern_excludes_the_project_directorys_children_at_every_depth() {
        assert_eq!(
            PROJECT_STATE_PATTERN,
            format!("{ANY_DEPTH}{PROJECT_DIRECTORY}/*")
        );
        assert!(!PROJECT_STATE_PATTERN.ends_with('/'));
        assert!(PROJECT_STATE_PATTERN.starts_with(ANY_DEPTH));
    }

    /// Every name here has to be one a loader actually reads from `<worktree>/.zuno/`,
    /// because the complement of this list is refused at delivery. Sorted and unique
    /// so a duplicate cannot render two identical negations and a new entry lands
    /// where a reader looks for it.
    #[test]
    fn the_user_owned_entries_are_unique_sorted_and_plain_names() {
        let mut sorted = USER_OWNED_ENTRIES.to_vec();
        sorted.sort_unstable();
        assert_eq!(USER_OWNED_ENTRIES, sorted.as_slice());
        sorted.dedup();
        assert_eq!(USER_OWNED_ENTRIES.len(), sorted.len(), "a duplicate entry");
        for entry in USER_OWNED_ENTRIES {
            assert!(
                !entry.contains('/') && !entry.contains('\\'),
                "an entry is one name directly under the project directory: {entry:?}"
            );
        }
        for expected in [
            CONFIG_FILE_STEM.to_owned() + ".json",
            CONFIG_FILE_STEM.to_owned() + ".jsonc",
        ] {
            assert!(
                USER_OWNED_ENTRIES.contains(&expected.as_str()),
                "the configuration chain reads {expected}, so it is the user's"
            );
        }
    }

    /// The compile-time check guards the rendered set; this pins what it refuses,
    /// since a `const` assertion cannot be exercised with a bad input.
    #[test]
    fn the_rendered_pattern_shape_check_accepts_an_exclusion_and_single_name_negations() {
        for accepted in [
            "**/.zuno/*",
            "!**/.zuno/zuno.json",
            "!**/.zuno/skills",
            "!**/.zuno/a",
        ] {
            assert!(is_rendered_pattern(accepted), "{accepted:?}");
        }
        for refused in [
            ".zuno/*",             // anchored at the root, so it misses `sub/.zuno/`
            "!.zuno/zuno.json",    // the negation would then miss it too
            "**/.zuno/",           // the project directory as a whole
            "**/.zuno/**",         // reaches deeper than the classifier does
            "**/.zuno/*/",         // a directory glob
            "!**/.zuno/*",         // re-includes everything the exclusion covered
            "!**/.zuno/skills/",   // a trailing slash git reads differently
            "!**/.zuno/a/b",       // a nested path
            "!**/.zuno/zuno.js?n", // a glob
            "!**/.zuno/[a]",       // a character class
            "!**/.zuno/a b",       // fine for git, but the block is space-separated nowhere
            "!**/.zuno/a\tb",      // whitespace git strips from the end
            "!**/.zuno/.",         // a dot segment
            "!**/.zuno/..",        // a dot segment
            "!**/.zuno/",          // a negation with no name
            "!/**/.zuno/a",        // anchored with a leading slash
            "!**/.zuno\\a",        // Windows separators
            "!**/.zunox/a",        // a different directory sharing the prefix
            "!**/a",               // not under the project directory
            "!*/.zuno/a",          // one level only, not every depth
            "!**.zuno/a",          // no separator after the depth prefix
            "!",
            "",
        ] {
            assert!(!is_rendered_pattern(refused), "{refused:?}");
        }
        assert!(is_negated("!**/.zuno/zuno.json"));
        assert!(!is_negated("**/.zuno/*"));
    }

    /// The compile-time check guards the registry; this pins down what it refuses,
    /// since a `const` assertion cannot be exercised with a bad input.
    #[test]
    fn the_pattern_shape_check_accepts_plain_directories_and_refuses_everything_else() {
        for accepted in [".zuno/goal/", ".zuno/tool-output/", ".zuno/a/b/"] {
            assert!(is_plain_directory_pattern(accepted), "{accepted:?}");
        }
        for refused in [
            ".zuno/",         // the project directory as a whole
            "/.zuno/goal/",   // anchored with a leading slash
            ".zuno/goal",     // would also match a file called `goal`
            ".zuno/*/",       // a glob
            ".zuno/go?l/",    // a glob
            ".zuno/[g]oal/",  // a glob
            ".zuno/!goal/",   // a negation character
            ".zuno/#goal/",   // a comment character
            ".zuno\\goal\\",  // Windows separators
            ".zuno/../goal/", // a dot segment
            ".zuno/./goal/",  // a dot segment
            ".zuno//goal/",   // an empty segment
            ".zuno/goal/\n",  // a line break
            "goal/",          // not under the project directory
            ".zunox/goal/",   // a different directory sharing the prefix
            "",
        ] {
            assert!(!is_plain_directory_pattern(refused), "{refused:?}");
        }
    }

    #[test]
    fn an_absolute_path_inside_the_worktree_is_classified() {
        assert_eq!(
            classify(worktree(), Path::new("/repo/.zuno/goal/ses_1.md")),
            Some(GeneratedReason::Registered(&GOAL_PROJECTION))
        );
        assert_eq!(
            classify(
                worktree(),
                Path::new("/repo/.zuno/tool-output/tool_ses_1_01")
            ),
            Some(GeneratedReason::Registered(&TOOL_OUTPUT))
        );
        assert_eq!(
            classify(
                worktree(),
                Path::new("/repo/.zuno/background/bg_1.status.json")
            ),
            Some(GeneratedReason::Registered(&BACKGROUND_EXECUTIONS))
        );
        assert!(is_generated(
            Path::new("/repo/"),
            Path::new("/repo/.zuno/goal/deeper/still.md")
        ));
    }

    /// A path from `git status --porcelain` is relative to the worktree root, and an
    /// untracked directory is printed with a trailing slash and no contents.
    #[test]
    fn a_path_relative_to_the_worktree_is_classified_whether_it_names_a_file_or_the_directory() {
        for path in [
            ".zuno/goal/ses_1.md",
            "./.zuno/goal/ses_1.md",
            ".zuno/goal/",
            ".zuno/goal",
        ] {
            assert_eq!(
                classify(worktree(), Path::new(path)),
                Some(GeneratedReason::Registered(&GOAL_PROJECTION)),
                "{path:?}"
            );
        }
        assert!(is_generated(
            Path::new("."),
            Path::new(".zuno/background/bg_1.output")
        ));
    }

    #[test]
    fn dot_and_dot_dot_components_are_resolved_without_touching_the_filesystem() {
        assert!(is_generated(
            worktree(),
            Path::new("src/../.zuno/goal/x.md")
        ));
        assert!(is_generated(
            worktree(),
            Path::new("/repo/src/./deep/../../.zuno/tool-output/t")
        ));
        assert!(is_generated(
            Path::new("/repo/./sub/.."),
            Path::new("/repo/.zuno/goal/x.md")
        ));
        assert!(
            !is_generated(worktree(), Path::new(".zuno/goal/../zuno.json")),
            "a path that leaves the generated directory is whatever it lands on"
        );
        assert!(
            !is_generated(worktree(), Path::new("/repo/.zuno/goal/../../src/lib.rs")),
            "climbing out of the project directory lands on source"
        );
        assert!(
            is_generated(Path::new("/"), Path::new("/../.zuno/goal/x.md")),
            "`..` at the root of an absolute path stays at the root"
        );
    }

    /// Nothing about a foreign path is an error: the answer is simply "not generated",
    /// and a caller iterating `git status` output must never be brought down by a
    /// path it did not expect.
    #[test]
    fn a_path_outside_the_worktree_is_not_generated_state_and_does_not_panic() {
        for path in [
            "/elsewhere/.zuno/goal/x.md",
            "/repo2/.zuno/goal/x.md",
            "/rep/.zuno/goal/x.md",
            "../.zuno/goal/x.md",
            "../repo/.zuno/goal/x.md",
            "/",
            "/repo",
            "",
            ".",
            "..",
        ] {
            assert_eq!(classify(worktree(), Path::new(path)), None, "{path:?}");
        }
        assert!(
            !is_generated(Path::new("."), Path::new("/repo/.zuno/goal/x.md")),
            "a relative worktree cannot anchor an absolute path without the filesystem"
        );
        assert!(!is_generated(
            Path::new("../gone"),
            Path::new("/repo/.zuno/goal/x.md")
        ));
    }

    /// The project directory's own name is compared exactly, so a path that is not
    /// under `.zuno/` is nobody's generated state.
    ///
    /// This test used to assert that `.zuno/Goal/`, `.zuno/goals/` and
    /// `.zuno/tool-outputs/` were *not* generated, because only a registered pattern
    /// counted. Under default deny they are: a misspelling of a registered directory
    /// is still Zuno's working state, and treating it as source is exactly how an
    /// unregistered directory got committed.
    ///
    /// It also used to assert that `sub/.zuno/goal/x.md` was not generated, back when
    /// only the worktree root's project directory counted. That expectation is gone:
    /// see `generated_state_a_release_left_in_a_subdirectory_is_still_generated_state`
    /// for why a project directory at any depth is read the same way.
    #[test]
    fn the_project_directorys_own_name_is_compared_exactly_so_neighbours_are_not_claimed() {
        for path in [".ZUNO/goal/x.md", "src/lib.rs", "zuno/goal/x.md"] {
            assert!(!is_generated(worktree(), Path::new(path)), "{path:?}");
        }
        for path in [
            ".zuno/Goal/x.md",
            ".zuno/goals/x.md",
            ".zuno/goal.md",
            ".zuno/tool-outputs/x",
        ] {
            assert_eq!(
                classify(worktree(), Path::new(path)),
                Some(GeneratedReason::UnregisteredProjectState),
                "a near-miss of a registered directory is still not source: {path:?}"
            );
        }
        assert_eq!(
            classify(worktree(), Path::new(".zuno/tool-output")),
            Some(GeneratedReason::Registered(&TOOL_OUTPUT)),
            "the registered directory itself is named, not merely covered"
        );
    }

    /// Default deny: a directory a release starts writing without registering it is
    /// generated state from the first write, not after somebody remembers the registry.
    #[test]
    fn anything_under_the_project_directory_that_is_not_the_users_is_generated_state() {
        for path in [
            ".zuno/whatever-comes-next/state.json",
            ".zuno/cache/blobs/ab/cd",
            ".zuno/scratch",
            ".zuno/index.db",
            "/repo/.zuno/telemetry/spans.jsonl",
        ] {
            assert_eq!(
                classify(worktree(), Path::new(path)),
                Some(GeneratedReason::UnregisteredProjectState),
                "{path:?}"
            );
        }
        let reason = classify(worktree(), Path::new(".zuno/scratch")).expect("generated");
        assert_eq!(reason.pattern(), PROJECT_STATE_PATTERN);
        assert_eq!(reason.registered(), None);
        assert!(
            !reason.reason().is_empty() && !reason.reason().ends_with('.'),
            "the reason is a clause for a report: {:?}",
            reason.reason()
        );
    }

    /// `docs/config/instructions.md` publishes `.zuno/rules/*.md` as the example
    /// `instructions` glob, so a team that followed it keeps hand-written source there.
    /// Default deny without that entry hid their files from `git status`, refused the
    /// commit that updated them, and dropped them out of new checkpoints — for a
    /// location Zuno's own documentation told them to use.
    #[test]
    fn an_instruction_file_the_configuration_guide_publishes_is_the_users_to_commit() {
        assert!(USER_OWNED_ENTRIES.contains(&"rules"));
        assert!(IGNORE_PATTERNS.contains(&"!**/.zuno/rules"));
        for path in [
            ".zuno/rules/house.md",
            "/repo/.zuno/rules/nested/deeper.md",
            "crates/foo/.zuno/rules/house.md",
        ] {
            assert_eq!(classify(worktree(), Path::new(path)), None, "{path:?}");
        }
        refuse_generated_state(worktree(), [".zuno/rules/house.md"])
            .expect("a documented instructions path is the user's own source");
    }

    /// A release that rooted generated state at the session's own directory left
    /// `sub/.zuno/tool-output/` and `sub/.zuno/background/` in checkouts that are still
    /// in use. Nothing rewrites or deletes them, so re-rooting the writers at the
    /// worktree fixes only the next write: the classifier and the rendered patterns are
    /// what keep the state already on disk out of a commit.
    ///
    /// The same depth carries the user's own configuration, because the configuration
    /// chain walks `.zuno` up from the session's directory — so the allow-list decides a
    /// nested project directory exactly as it decides the root one.
    #[test]
    fn generated_state_a_release_left_in_a_subdirectory_is_still_generated_state() {
        assert_eq!(
            classify(
                worktree(),
                Path::new("crates/foo/.zuno/tool-output/tool_ses_abc.md")
            ),
            Some(GeneratedReason::Registered(&TOOL_OUTPUT))
        );
        assert_eq!(
            classify(worktree(), Path::new("/repo/sub/.zuno/goal/ses_1.md")),
            Some(GeneratedReason::Registered(&GOAL_PROJECTION))
        );
        assert_eq!(
            classify(worktree(), Path::new("sub/.zuno/whatever/state.json")),
            Some(GeneratedReason::UnregisteredProjectState)
        );
        for users in [
            "sub/.zuno/zuno.json",
            "crates/foo/.zuno/skill/review/SKILL.md",
            "crates/foo/.zuno/agent/build.md",
            "sub/.zuno",
            "sub/.zuno/",
        ] {
            assert_eq!(
                classify(worktree(), Path::new(users)),
                None,
                "the configuration chain reads a nested project directory too: {users:?}"
            );
        }
        assert_eq!(
            classify(worktree(), Path::new(".zuno/goal/.zuno/tool-output/x")),
            Some(GeneratedReason::Registered(&GOAL_PROJECTION)),
            "the outermost project directory decides, so a name below one cannot reopen it"
        );
    }

    /// Git folds ASCII case wherever `core.ignorecase` is set — every Windows and
    /// macOS checkout — so `!.zuno/zuno.json` re-includes `.zuno/Zuno.json` there.
    /// Calling it generated would refuse a commit git had already un-ignored.
    #[test]
    fn a_user_owned_entry_is_the_users_whatever_its_ascii_case() {
        for path in [
            ".zuno/Zuno.json",
            ".zuno/ZUNO.JSONC",
            ".zuno/SKILLS/review/SKILL.md",
            ".zuno/Agents/build.md",
            ".zuno/rules.md",
            ".zuno/Plans/1700000000000-swift-otter.md",
        ] {
            assert!(
                !is_generated(worktree(), Path::new(path)),
                "an ambiguous spelling is resolved in the user's favour: {path:?}"
            );
        }
    }

    /// The project directory holds the user's configuration, and every one of these
    /// is something a user commits. Claiming any of them would hide a real change.
    #[test]
    fn the_users_own_configuration_under_the_project_directory_is_never_generated() {
        for path in [
            ".zuno",
            ".zuno/",
            ".zuno/zuno.json",
            ".zuno/zuno.jsonc",
            ".zuno/tui.json",
            ".zuno/RULES.md",
            ".zuno/skills/review/SKILL.md",
            ".zuno/skill/review/SKILL.md",
            ".zuno/agents/build.md",
            ".zuno/command/ship.md",
            ".zuno/extensions/plugin/extension.json",
            ".zuno/plans/1700000000000-swift-otter.md",
        ] {
            assert_eq!(classify(worktree(), Path::new(path)), None, "{path:?}");
        }
    }

    /// `PathBuf::join` uses the platform separator, so this is a backslash path on
    /// Windows and a slash path everywhere else; both must match the slash pattern.
    #[test]
    fn a_platform_joined_path_matches_the_slash_separated_pattern() {
        let root = std::env::temp_dir().join("zuno-generated-paths-test");
        let document = root.join(".zuno").join("goal").join("ses_1.md");
        assert_eq!(
            classify(&root, &document),
            Some(GeneratedReason::Registered(&GOAL_PROJECTION))
        );
        assert!(!is_generated(&root, &root.join(".zuno").join("zuno.json")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_backslash_paths_and_verbatim_prefixes_match() {
        let worktree = Path::new(r"C:\repo");
        assert!(is_generated(
            worktree,
            Path::new(r"C:\repo\.zuno\goal\ses_1.md")
        ));
        assert!(is_generated(worktree, Path::new(r".zuno\goal\ses_1.md")));
        assert!(is_generated(worktree, Path::new(".zuno/goal/ses_1.md")));
        assert!(is_generated(
            Path::new(r"\\?\C:\repo"),
            Path::new(r"c:\repo\.zuno\goal\ses_1.md")
        ));
        assert!(is_generated(
            Path::new(r"C:\repo"),
            Path::new(r"\\?\C:\repo\.zuno\background\bg_1.output")
        ));
        assert!(!is_generated(
            worktree,
            Path::new(r"D:\repo\.zuno\goal\ses_1.md")
        ));
        assert!(
            !is_generated(worktree, Path::new(r"C:.zuno\goal\ses_1.md")),
            "a drive-relative path cannot be placed without the current directory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn on_unix_a_backslash_is_an_ordinary_filename_byte_and_not_a_separator() {
        assert!(!is_generated(worktree(), Path::new(r".zuno\goal\ses_1.md")));
    }

    /// `Prefix` values can be built on any platform even though only Windows ever
    /// produces one, so the folding is checked everywhere.
    #[test]
    fn equivalent_windows_volume_prefixes_compare_equal_and_different_ones_do_not() {
        assert!(same_prefix(None, None));
        assert!(same_prefix(
            Some(Prefix::VerbatimDisk(b'C')),
            Some(Prefix::Disk(b'c'))
        ));
        assert!(same_prefix(
            Some(Prefix::UNC(OsStr::new("Server"), OsStr::new("share"))),
            Some(Prefix::VerbatimUNC(
                OsStr::new("server"),
                OsStr::new("SHARE")
            ))
        ));
        assert!(!same_prefix(
            Some(Prefix::Disk(b'C')),
            Some(Prefix::Disk(b'D'))
        ));
        assert!(!same_prefix(Some(Prefix::Disk(b'C')), None));
        assert!(!same_prefix(
            Some(Prefix::Disk(b'C')),
            Some(Prefix::UNC(OsStr::new("server"), OsStr::new("share")))
        ));
        assert!(!same_prefix(
            Some(Prefix::Verbatim(OsStr::new("pictures"))),
            Some(Prefix::Verbatim(OsStr::new("music")))
        ));
        assert!(same_prefix(
            Some(Prefix::DeviceNS(OsStr::new("COM1"))),
            Some(Prefix::DeviceNS(OsStr::new("COM1")))
        ));
    }

    #[test]
    fn describe_reads_as_one_line_naming_the_pattern_the_reason_and_the_scope() {
        let line = GOAL_PROJECTION.describe();
        assert!(line.starts_with("`.zuno/goal/` — "), "{line}");
        assert!(line.contains(GOAL_PROJECTION.reason), "{line}");
        assert!(line.ends_with("(one entry per session)"), "{line}");
        assert!(
            BACKGROUND_EXECUTIONS
                .describe()
                .ends_with("(workspace-wide)")
        );
        for generated in GENERATED_PATHS {
            assert!(!generated.describe().contains('\n'));
        }
    }

    #[test]
    fn a_delivery_with_no_generated_state_passes() {
        refuse_generated_state(
            worktree(),
            [
                "src/lib.rs",
                "README.md",
                ".zuno/zuno.json",
                ".zuno/plans/p.md",
            ],
        )
        .expect("nothing generated was staged");
        refuse_generated_state(worktree(), Vec::<PathBuf>::new()).expect("an empty list passes");
    }

    #[test]
    fn staged_generated_state_is_refused_naming_every_path_its_reason_and_the_remedy() {
        let refusal = refuse_generated_state(
            worktree(),
            [
                "src/lib.rs",
                ".zuno/goal/ses_1.md",
                ".zuno/zuno.json",
                "/repo/.zuno/background/bg_1.status.json",
                ".zuno/tool-output/",
            ],
        )
        .expect_err("generated state was staged");

        assert_eq!(
            refusal.offending,
            vec![
                StagedGeneratedPath {
                    path: PathBuf::from(".zuno/goal/ses_1.md"),
                    generated: GeneratedReason::Registered(&GOAL_PROJECTION),
                },
                StagedGeneratedPath {
                    path: PathBuf::from("/repo/.zuno/background/bg_1.status.json"),
                    generated: GeneratedReason::Registered(&BACKGROUND_EXECUTIONS),
                },
                StagedGeneratedPath {
                    path: PathBuf::from(".zuno/tool-output/"),
                    generated: GeneratedReason::Registered(&TOOL_OUTPUT),
                },
            ],
            "only the generated paths, in the order they were reported"
        );

        let report = refusal.report();
        assert!(report.starts_with("3 staged paths are"), "{report}");
        for staged in &refusal.offending {
            assert!(
                report.contains(&staged.path.display().to_string()),
                "{report}"
            );
            assert!(report.contains(staged.generated.reason()), "{report}");
        }
        assert!(report.contains(GeneratedStateStaged::REMEDY), "{report}");
        assert!(report.contains("force-added"), "{report}");
        assert!(report.contains("git restore --staged"), "{report}");
        assert!(
            report.contains("git rm --cached"),
            "an ignore rule never applies to a tracked path, so unstaging alone leaves \
             the next `git commit -a` delivering it again: {report}"
        );

        let line = refusal.to_string();
        assert!(!line.contains('\n'), "{line}");
        assert!(
            line.contains(".zuno/goal/ses_1.md (matches `.zuno/goal/`)"),
            "{line}"
        );
        assert!(
            line.contains("bg_1.status.json (matches `.zuno/background/`)"),
            "{line}"
        );
    }

    #[test]
    fn a_single_offending_path_is_reported_in_the_singular() {
        let refusal = refuse_generated_state(worktree(), [".zuno/goal/ses_1.md"])
            .expect_err("generated state was staged");
        assert!(
            refusal.report().starts_with("1 staged path is"),
            "{}",
            refusal.report()
        );
    }

    /// A refusal for an unregistered path has to tell the user where a file of their
    /// own goes instead, or it reads as a bug in the check rather than as an answer.
    #[test]
    fn a_refusal_for_unregistered_project_state_names_the_entries_that_are_the_users() {
        let refusal = refuse_generated_state(worktree(), [".zuno/whatever/state.json"])
            .expect_err("state under the project directory is not source");

        let report = refusal.report();
        assert!(
            report.contains(GeneratedStateStaged::UNREGISTERED_REMEDY),
            "{report}"
        );
        assert!(
            !report.contains(GeneratedStateStaged::REMEDY),
            "the registered remedy talks about a `.gitignore` this path has none of: \
             {report}"
        );
        assert!(report.contains("keep it outside"), "{report}");
        assert!(
            refusal
                .to_string()
                .contains("state.json (matches `**/.zuno/*`)"),
            "{refusal}"
        );
    }

    /// The one test that lets git settle it: the pattern set has to hide every
    /// generated directory, registered or not, and leave every entry a person authors
    /// visible in the same project directory.
    ///
    /// It replaces an assertion that only checked the three registered directories,
    /// which is what let an unregistered one be committed.
    #[test]
    fn the_exclude_patterns_hide_generated_state_from_git_and_leave_the_users_entries_visible() {
        let root = repository();
        let project = root.path().join(PROJECT_DIRECTORY);
        for generated in GENERATED_PATHS {
            let directory = generated
                .segments()
                .fold(root.path().to_path_buf(), |path, segment| {
                    path.join(segment)
                });
            fs::create_dir_all(&directory).expect("create the generated directory");
            fs::write(directory.join("entry"), "generated\n").expect("write a generated entry");
            assert!(is_generated(root.path(), &directory.join("entry")));
        }
        // Nobody registered these. Under default deny they are hidden all the same.
        for unregistered in ["whatever-comes-next", "cache"] {
            let directory = project.join(unregistered);
            fs::create_dir_all(&directory).expect("create an unregistered directory");
            fs::write(directory.join("state.json"), "{}\n").expect("write unregistered state");
        }
        fs::write(project.join("index.db"), "binary\n").expect("write an unregistered file");
        // And every entry a person authors, one per rendered negation.
        let mut expected = Vec::new();
        for entry in USER_OWNED_ENTRIES {
            if entry.contains('.') {
                fs::write(project.join(entry), "{}\n").expect("write the user's file");
                expected.push(format!("?? {PROJECT_DIRECTORY}/{entry}"));
            } else {
                let directory = project.join(entry);
                fs::create_dir_all(&directory).expect("create the user's directory");
                fs::write(directory.join("mine.md"), "# mine\n").expect("write the user's file");
                expected.push(format!("?? {PROJECT_DIRECTORY}/{entry}/mine.md"));
            }
        }
        expected.sort_unstable();
        assert!(
            !run(root.path(), &["git", "status", "--porcelain"]).is_empty(),
            "the fixture must be dirty before the exclusion, or it proves nothing"
        );

        ensure_managed_block(root.path(), IGNORE_PATTERNS).expect("write the block");

        let status = run(
            root.path(),
            &["git", "status", "--porcelain", "--untracked-files=all"],
        );
        let mut reported: Vec<String> = status.lines().map(str::to_owned).collect();
        reported.sort_unstable();
        assert_eq!(
            reported, expected,
            "generated state hidden, registered or not; every user-owned entry visible"
        );
    }

    /// The other half of the same question, settled by git: a project directory in a
    /// subdirectory.
    ///
    /// Releases before the writers were re-rooted wrote `sub/.zuno/tool-output/` and
    /// `sub/.zuno/background/`, and a root-anchored `.zuno/*` said nothing about either,
    /// so `git add -A` collected them. The configuration chain reads `sub/.zuno/zuno.json`
    /// at that same depth, so the negations have to reach it too.
    #[test]
    fn the_exclude_patterns_hide_generated_state_in_a_subdirectory_and_leave_its_config_visible() {
        let root = repository();
        let project = root
            .path()
            .join("crates")
            .join("foo")
            .join(PROJECT_DIRECTORY);
        for generated in GENERATED_PATHS {
            let directory = generated
                .segments()
                .fold(root.path().join("crates").join("foo"), |path, segment| {
                    path.join(segment)
                });
            fs::create_dir_all(&directory).expect("create the generated directory");
            fs::write(directory.join("entry"), "generated\n").expect("write a generated entry");
            assert!(is_generated(root.path(), &directory.join("entry")));
        }
        let unregistered = project.join("whatever-comes-next");
        fs::create_dir_all(&unregistered).expect("create an unregistered directory");
        fs::write(unregistered.join("state.json"), "{}\n").expect("write unregistered state");
        fs::write(project.join("zuno.json"), "{}\n").expect("write the user's configuration");
        let rules = project.join("rules");
        fs::create_dir_all(&rules).expect("create the user's rules directory");
        fs::write(rules.join("house.md"), "# house\n").expect("write the user's rule");

        ensure_managed_block(root.path(), IGNORE_PATTERNS).expect("write the block");

        let status = run(
            root.path(),
            &["git", "status", "--porcelain", "--untracked-files=all"],
        );
        let mut reported: Vec<String> = status.lines().map(str::to_owned).collect();
        reported.sort_unstable();
        assert_eq!(
            reported,
            vec![
                format!("?? crates/foo/{PROJECT_DIRECTORY}/rules/house.md"),
                format!("?? crates/foo/{PROJECT_DIRECTORY}/zuno.json"),
            ],
            "generated state hidden at every depth; the configuration read at that \
             depth still visible"
        );
    }
}

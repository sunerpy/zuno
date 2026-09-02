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
//! [`GENERATED_PATHS`] is the single source of truth. One entry per runtime path, each
//! with the git pattern that hides it, the reason it exists, and whether its entries
//! belong to a session. Everything else here is derived from it: [`IGNORE_PATTERNS`] is
//! what the host hands to [`crate::ensure_managed_block`], [`is_generated`] answers
//! whether a path is generated state, and [`refuse_generated_state`] is the delivery
//! check that keeps such a path out of a commit.
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
//! # Why `.zuno/` as a whole is not registered
//!
//! The project directory also holds `zuno.json`, skills, agents, commands, extensions
//! and `RULES.md`: configuration a user writes and commits. A single `.zuno/` pattern
//! would hide a user's own edit to their configuration from `git status`, and the
//! delivery check would refuse to commit it. Only the subdirectories Zuno *generates*
//! are listed, each proven by the code that writes it. Plans under `.zuno/plans/` are
//! deliberately absent too: the model writes them through the ordinary `write` tool
//! under the user's permission rules, and `zuno-agent`'s `plan_file` documents them as
//! a path the user may gitignore or commit as they choose.
//!
//! # Why the check can be lexical
//!
//! Every registered pattern is a plain directory prefix — `.zuno/<name>/`, slash
//! separated, no glob characters, a trailing slash so it names a directory and not a
//! file that shares the name. That is the whole reason [`is_generated`] can be a
//! component-wise comparison instead of a gitignore matcher: git and this module read a
//! prefix the same way. The shape is enforced at compile time, so a pattern that would
//! make the two disagree cannot be registered.
//!
//! ```
//! use std::path::Path;
//! use zuno_paths::generated::{is_generated, refuse_generated_state};
//!
//! let worktree = Path::new("/repo");
//! assert!(is_generated(worktree, Path::new(".zuno/goal/ses_1.md")));
//! assert!(is_generated(worktree, Path::new("/repo/src/../.zuno/tool-output/tool_x")));
//! assert!(!is_generated(worktree, Path::new(".zuno/zuno.json")));
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
    /// The git pattern that hides the path: a directory relative to the worktree root,
    /// slash-separated, with a trailing slash.
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
    fn segments(&self) -> impl Iterator<Item = &'static str> {
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

/// `.zuno/background/<id>.status.json` and `<id>.output`, the shell tool's background
/// commands.
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
pub const GENERATED_PATHS: &[GeneratedPath] =
    &[GOAL_PROJECTION, TOOL_OUTPUT, BACKGROUND_EXECUTIONS];

const PATTERN_COUNT: usize = GENERATED_PATHS.len();

const fn ignore_pattern_array() -> [&'static str; PATTERN_COUNT] {
    let mut patterns = [""; PATTERN_COUNT];
    let mut index = 0;
    while index < PATTERN_COUNT {
        patterns[index] = GENERATED_PATHS[index].pattern;
        index += 1;
    }
    patterns
}

const IGNORE_PATTERN_ARRAY: [&str; PATTERN_COUNT] = ignore_pattern_array();

/// Every registered pattern, in registry order, ready for [`crate::ensure_managed_block`].
///
/// Derived from [`GENERATED_PATHS`] at compile time rather than listed a second time,
/// so the exclude block and the registry cannot name different paths.
pub const IGNORE_PATTERNS: &[&str] = &IGNORE_PATTERN_ARRAY;

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

/// The registry entry that covers `path`, or `None` when `path` is not generated state.
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
/// reads it too. Names are compared exactly: Zuno writes these directories in the
/// registered spelling, and a case-folded match would claim a user's own `.Zuno/`
/// directory on a case-sensitive filesystem.
#[must_use]
pub fn classify(worktree: &Path, path: &Path) -> Option<&'static GeneratedPath> {
    let relative = relative_to_worktree(worktree, path)?;
    GENERATED_PATHS
        .iter()
        .find(|generated| generated.covers(&relative))
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
    /// The registry entry it falls under, which names the reason it exists.
    pub generated: &'static GeneratedPath,
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
    /// The exclude block normally keeps these paths out of the index altogether, so a
    /// generated path reaching a commit has exactly two causes, and the remedy names
    /// both: the block was removed, or the file was added with `--force`.
    pub const REMEDY: &'static str = "Unstage each one (`git restore --staged -- <path>`) \
        and leave it out of the commit. The repository-private exclude block should already \
        have hidden it, so it is here because the block was removed or the file was \
        force-added.";

    /// The human-facing report: one line per path with its reason, then the remedy.
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
                staged.generated.reason
            ));
        }
        report.push_str(Self::REMEDY);
        report.push('\n');
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
                staged.generated.pattern
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
    use crate::{ensure_managed_block, files::BACKGROUND_DIRECTORY, files::TOOL_OUTPUT_DIRECTORY};
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

    #[test]
    fn ignore_patterns_are_the_registry_patterns_in_registry_order() {
        let expected: Vec<&str> = GENERATED_PATHS
            .iter()
            .map(|generated| generated.pattern)
            .collect();
        assert_eq!(IGNORE_PATTERNS, expected.as_slice());
        for pattern in IGNORE_PATTERNS {
            assert!(
                !pattern.contains(['\n', '\r']),
                "`ensure_managed_block` refuses a multi-line entry: {pattern:?}"
            );
        }
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
            Some(&GOAL_PROJECTION)
        );
        assert_eq!(
            classify(
                worktree(),
                Path::new("/repo/.zuno/tool-output/tool_ses_1_01")
            ),
            Some(&TOOL_OUTPUT)
        );
        assert_eq!(
            classify(
                worktree(),
                Path::new("/repo/.zuno/background/bg_1.status.json")
            ),
            Some(&BACKGROUND_EXECUTIONS)
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
                Some(&GOAL_PROJECTION),
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

    #[test]
    fn names_are_compared_exactly_so_neighbours_and_lookalikes_are_not_claimed() {
        for path in [
            ".ZUNO/goal/x.md",
            ".zuno/Goal/x.md",
            ".zuno/goals/x.md",
            ".zuno/goal.md",
            ".zuno/tool-outputs/x",
            "sub/.zuno/goal/x.md",
        ] {
            assert!(!is_generated(worktree(), Path::new(path)), "{path:?}");
        }
        assert_eq!(
            classify(worktree(), Path::new(".zuno/tool-output")),
            Some(&TOOL_OUTPUT),
            "the directory itself is generated state, only its lookalikes are not"
        );
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
        assert_eq!(classify(&root, &document), Some(&GOAL_PROJECTION));
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
                    generated: &GOAL_PROJECTION,
                },
                StagedGeneratedPath {
                    path: PathBuf::from("/repo/.zuno/background/bg_1.status.json"),
                    generated: &BACKGROUND_EXECUTIONS,
                },
                StagedGeneratedPath {
                    path: PathBuf::from(".zuno/tool-output/"),
                    generated: &TOOL_OUTPUT,
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
            assert!(report.contains(staged.generated.reason), "{report}");
        }
        assert!(report.contains(GeneratedStateStaged::REMEDY), "{report}");
        assert!(report.contains("force-added"), "{report}");
        assert!(report.contains("git restore --staged"), "{report}");

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

    /// The registry, through the real exclude mechanism, must hide exactly the
    /// generated directories: a user's own configuration in the same project
    /// directory has to stay visible to `git status`.
    #[test]
    fn the_registry_patterns_hide_every_generated_path_from_git_and_nothing_else() {
        let root = repository();
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
        fs::write(root.path().join(".zuno").join("zuno.json"), "{}\n")
            .expect("write the user's configuration");
        assert!(
            !run(root.path(), &["git", "status", "--porcelain"]).is_empty(),
            "the fixture must be dirty before the exclusion, or it proves nothing"
        );

        ensure_managed_block(root.path(), IGNORE_PATTERNS).expect("write the block");

        let status = run(
            root.path(),
            &["git", "status", "--porcelain", "--untracked-files=all"],
        );
        assert_eq!(
            status.trim(),
            "?? .zuno/zuno.json",
            "every generated directory hidden, the user's configuration still visible"
        );
    }
}

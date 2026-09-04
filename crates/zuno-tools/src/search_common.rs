//! The pieces `glob` and `grep` share: scope resolution, the escalation for a path
//! outside the workspace, and the error mapping.
//!
//! Kept out of both tools so the two cannot drift on the parts a caller can observe
//! — the permission key, the escalation pattern, and which failures are
//! model-correctable.

use serde_json::json;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use zuno_error::ToolError;
use zuno_search::{Cancellation, Ripgrep, SearchError};
use zuno_tool::{InterruptHandle, PermissionAsk, ToolContext};

/// The result cap both tools apply.
///
/// `100`, hard-coded at `packages/opencode/src/tool/glob.ts:49` and
/// `grep.ts:67,80`. Not configurable upstream, so not configurable here.
pub const RESULT_LIMIT: usize = 100;

/// Where a search is rooted when the call does not say.
///
/// The oracle reads these off `InstanceState.context` (`glob.ts:27`, `grep.ts:50`):
/// `directory` is the session's working directory and `worktree` is the repository
/// root, which is only used to render `glob`'s title. They are separate because a
/// session can be opened in a subdirectory of a repository, and the title is then
/// the subdirectory's path relative to the root rather than an absolute path.
#[derive(Debug, Clone)]
pub struct SearchScope {
    /// The default search root, and what a relative `path` argument resolves against.
    pub directory: PathBuf,
    /// The repository root, used only to render `glob`'s title.
    pub worktree: PathBuf,
}

impl SearchScope {
    /// A scope whose directory is also its worktree.
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        let directory = directory.into();
        Self {
            worktree: directory.clone(),
            directory,
        }
    }

    /// A scope with a distinct worktree root.
    #[must_use]
    pub fn with_worktree(directory: impl Into<PathBuf>, worktree: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            worktree: worktree.into(),
        }
    }

    /// Resolves a `path` argument the way both tools do.
    ///
    /// An absolute argument is taken as given; a relative one joins
    /// [`SearchScope::directory`]; an absent one *is* the directory. Matches
    /// `glob.ts:38-39` and `grep.ts:51-53`.
    #[must_use]
    pub fn resolve(&self, requested: Option<&str>) -> PathBuf {
        match requested {
            Some(path) if Path::new(path).is_absolute() => PathBuf::from(path),
            Some(path) => normalize(&self.directory.join(path)),
            None => self.directory.clone(),
        }
    }

    /// Whether `target` is inside the session directory or the worktree.
    ///
    /// The oracle's `containsPath(full, ins)` checks the instance's directory *and*
    /// its worktree (`project/instance-context.ts`), so a session opened in a
    /// subdirectory can still reach a sibling directory of the same repository
    /// without an escalation.
    #[must_use]
    pub fn contains(&self, target: &Path) -> bool {
        let target = normalize(target);
        target.starts_with(normalize(&self.directory))
            || target.starts_with(normalize(&self.worktree))
    }
}

/// Collapses `.` and `..` without touching the filesystem.
///
/// `std::path::Path` has no such operation and [`std::fs::canonicalize`] would
/// require the path to exist, which a search root given by a model may not. The
/// oracle's `FSUtil.resolve` calls `realpathSync` and falls back to the lexical
/// result on `ENOENT`; the lexical result is what matters for the containment check,
/// so that is what this computes.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// What a path being escalated is, which decides the directory the grant covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    /// The path is a directory; the grant covers it.
    Directory,
    /// The path is a file, or does not exist; the grant covers its parent.
    File,
}

/// The one spelling of a directory grant pattern, shared by every tool.
///
/// [`zuno_paths::wire_path`] rather than a local separator replacement: it also drops
/// Windows' verbatim `\\?\` prefix. The shell tool rendered a canonicalized directory
/// with [`std::path::MAIN_SEPARATOR`] and so asked for `\\?\C:\dir\*`, while the file
/// and search tools asked for `C:/dir/*` and a user rule could only be written one of
/// those two ways. Every grant now names the same pattern, so one standing
/// `external_directory` decision covers a directory for all of them.
///
/// The pattern is built by joining, not by concatenating a separator, so a grant on a
/// filesystem root stays `/*` instead of becoming `//*`.
#[must_use]
pub fn directory_grant_pattern(directory: &Path) -> String {
    zuno_paths::wire_path(&directory.join("*"))
}

/// Raises the `external_directory` escalation for a path outside the workspace.
///
/// A faithful port of `tool/external-directory.ts:15-44`: the permission key is
/// `external_directory`, the pattern is the target directory with `/*` appended, and
/// `always` offers a standing grant for that same directory. Returns whether an
/// escalation was raised, which is what the oracle returns.
///
/// # Errors
///
/// [`ToolError::Denied`] when the escalation is refused.
pub async fn assert_external_directory(
    ctx: &ToolContext,
    tool: &str,
    scope: &SearchScope,
    target: &Path,
    kind: TargetKind,
) -> Result<bool, ToolError> {
    if scope.contains(target) {
        return Ok(false);
    }

    let full = normalize(target);
    let directory = match kind {
        TargetKind::Directory => full.clone(),
        TargetKind::File => full.parent().unwrap_or(&full).to_path_buf(),
    };
    let pattern = directory_grant_pattern(&directory);

    let mut ask = PermissionAsk::new("external_directory", pattern.clone());
    ask.always = vec![pattern];
    ask.metadata = json!({
        "filepath": full.to_string_lossy(),
        "parentDir": directory.to_string_lossy(),
    })
    .as_object()
    .cloned()
    .unwrap_or_default();

    ctx.ask(tool, ask).await?;
    Ok(true)
}

/// Adapts a tool's interrupt to the search engine's cancellation signal.
///
/// One forwarding method, so the search crate does not depend on the tool layer and
/// the LSP walk in todo 48 can reuse the engine without it.
pub struct InterruptCancellation(pub Arc<dyn InterruptHandle>);

impl Cancellation for InterruptCancellation {
    fn is_cancelled(&self) -> bool {
        self.0.is_set()
    }
}

impl InterruptCancellation {
    /// Wraps the interrupt carried by `ctx`.
    #[must_use]
    pub fn from_context(ctx: &ToolContext) -> Self {
        Self(Arc::clone(&ctx.interrupt))
    }
}

/// Maps a search failure onto the tool error taxonomy.
///
/// The split follows [`SearchError::is_model_correctable`]: a bad pattern or a file
/// where a directory was wanted becomes [`ToolError::InvalidArgs`], because the
/// model can issue a different call; everything else becomes [`ToolError::Failed`],
/// because nothing the model writes next will change it.
#[must_use]
pub fn map_search_error(tool: &str, error: SearchError) -> ToolError {
    if error.is_model_correctable() {
        ToolError::InvalidArgs {
            tool: tool.to_owned(),
            source: Box::new(error),
        }
    } else {
        ToolError::Failed {
            tool: tool.to_owned(),
            source: Box::new(error),
        }
    }
}

/// The official ripgrep adapter both tools run their requests through.
///
/// Held by the tool so discovery and version validation are shared by both tools.
#[derive(Debug, Clone)]
pub struct SearchTooling {
    /// Where searches are rooted.
    pub scope: SearchScope,
    /// Which `rg` executable answers them.
    pub ripgrep: Ripgrep,
}

impl SearchTooling {
    /// Tooling rooted at `directory`, deferring PATH failure until invocation.
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            scope: SearchScope::new(directory),
            ripgrep: Ripgrep::deferred_system(),
        }
    }

    /// Tooling with a complete scope, deferring ripgrep discovery until use.
    #[must_use]
    pub fn deferred(scope: SearchScope) -> Self {
        Self {
            scope,
            ripgrep: Ripgrep::deferred_system(),
        }
    }

    /// Resolve and version-check the official `rg` executable for one scope.
    pub fn discover(scope: SearchScope) -> Result<Self, SearchError> {
        Ok(Self {
            scope,
            ripgrep: Ripgrep::discover()?,
        })
    }

    /// Tooling with an explicit scope and executable.
    #[must_use]
    pub fn with_ripgrep(scope: SearchScope, ripgrep: Ripgrep) -> Self {
        Self { scope, ripgrep }
    }
}

/// Renders `path` relative to `base`, falling back to the absolute path.
///
/// `path.relative(ins.worktree, search)` in `glob.ts:66`. Node returns `""` when the
/// two are equal, and so does this, because that empty title is what the oracle
/// shows for a search at the repository root.
#[must_use]
pub fn display_relative(base: &Path, path: &Path) -> String {
    let base = normalize(base);
    let path = normalize(path);
    match path.strip_prefix(&base) {
        Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
        Err(_) => path.to_string_lossy().replace('\\', "/"),
    }
}

/// `path` as one line of model-visible text.
///
/// A path is an identifier the model feeds straight back into `read`, `edit` and the
/// next search, and both search tools render one identifier per line — so a file name
/// that carries a line break read as two files, neither of which existed. Control
/// characters, and the Unicode line and paragraph separators some readers also break
/// on, are spelled the way Rust's `Debug` spells them (`\n`, `\t`, `\u{7f}`). Nothing
/// else is touched: no quoting and no escaping of `\`, `'` or `"`, so an ordinary path,
/// a Windows path included, renders byte-for-byte as before and only a name that could
/// not otherwise be shown on one line changes. `Debug` spelling rather than quoting
/// because quoting would change every line the model has been trained on, and because
/// the escape is reversible for a reader who needs the original. The structured
/// metadata beside the text keeps the exact bytes; this spelling is for reading.
#[must_use]
pub fn one_line(path: &Path) -> String {
    let text = path.to_string_lossy();
    if !text.chars().any(breaks_a_line) {
        return text.into_owned();
    }
    let mut rendered = String::with_capacity(text.len() + 8);
    for character in text.chars() {
        if breaks_a_line(character) {
            rendered.extend(character.escape_debug());
        } else {
            rendered.push(character);
        }
    }
    rendered
}

fn breaks_a_line(character: char) -> bool {
    character.is_control() || matches!(character, '\u{2028}' | '\u{2029}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_without_control_characters_renders_byte_for_byte() {
        for path in [
            "/work/project/src/main.rs",
            r"C:\Users\alice\src\main.rs",
            "/work/it's \"quoted\"/back\\slash.rs",
            "/work/caf\u{e9}/\u{200b}zero-width.rs",
        ] {
            assert_eq!(one_line(Path::new(path)), path);
        }
    }

    #[test]
    fn a_line_break_in_a_file_name_is_spelled_so_one_file_is_one_line() {
        let rendered = one_line(Path::new("/work/two\nlines\r\ttab\u{7f}\u{2028}.ts"));
        assert_eq!(rendered, r"/work/two\nlines\r\ttab\u{7f}\u{2028}.ts");
        assert_eq!(rendered.lines().count(), 1);
    }

    #[test]
    fn an_absent_path_argument_resolves_to_the_session_directory() {
        let scope = SearchScope::new("/work/project");
        assert_eq!(scope.resolve(None), PathBuf::from("/work/project"));
    }

    #[test]
    fn a_relative_path_argument_joins_the_session_directory() {
        let scope = SearchScope::new("/work/project");
        assert_eq!(
            scope.resolve(Some("src/tool")),
            PathBuf::from("/work/project/src/tool")
        );
        assert_eq!(scope.resolve(Some(".")), PathBuf::from("/work/project"));
        assert_eq!(
            scope.resolve(Some("../other")),
            PathBuf::from("/work/other")
        );
    }

    #[test]
    fn an_absolute_path_argument_is_taken_as_given() {
        let scope = SearchScope::new("/work/project");
        assert_eq!(
            scope.resolve(Some("/elsewhere")),
            PathBuf::from("/elsewhere")
        );
    }

    #[test]
    fn containment_covers_both_the_directory_and_the_worktree() {
        let scope = SearchScope::with_worktree("/repo/packages/app", "/repo");

        assert!(scope.contains(Path::new("/repo/packages/app/src")));
        assert!(
            scope.contains(Path::new("/repo/packages/other")),
            "a sibling package of the same repository is inside the worktree"
        );
        assert!(!scope.contains(Path::new("/elsewhere")));
    }

    #[test]
    fn a_traversal_out_of_the_workspace_is_not_treated_as_contained() {
        let scope = SearchScope::new("/repo");
        assert!(
            !scope.contains(Path::new("/repo/../secrets")),
            "the lexical collapse must happen before the prefix test"
        );
    }

    #[test]
    fn a_title_at_the_worktree_root_is_empty_exactly_as_node_renders_it() {
        assert_eq!(display_relative(Path::new("/repo"), Path::new("/repo")), "");
        assert_eq!(
            display_relative(Path::new("/repo"), Path::new("/repo/src")),
            "src"
        );
        assert_eq!(
            display_relative(Path::new("/repo"), Path::new("/elsewhere")),
            "/elsewhere"
        );
    }

    #[test]
    fn a_model_correctable_search_failure_stays_model_correctable() {
        let mapped = map_search_error(
            "grep",
            SearchError::InvalidPattern {
                pattern: "(".to_owned(),
                message: "unclosed group".to_owned(),
            },
        );
        assert!(matches!(mapped, ToolError::InvalidArgs { .. }));
        assert!(mapped.is_model_correctable());

        let fatal = map_search_error(
            "grep",
            SearchError::RootMissing {
                root: PathBuf::from("/nowhere"),
            },
        );
        assert!(matches!(fatal, ToolError::Failed { .. }));
        assert!(!fatal.is_model_correctable());
    }
}

#[cfg(test)]
mod grant_pattern_tests {
    use super::*;

    #[test]
    fn a_grant_pattern_uses_forward_slashes_and_one_wildcard_segment() {
        let directory = Path::new("/srv").join("data").join("shared");
        assert_eq!(directory_grant_pattern(&directory), "/srv/data/shared/*");
    }

    #[cfg(unix)]
    #[test]
    fn a_grant_on_a_root_directory_does_not_double_its_separator() {
        assert_eq!(directory_grant_pattern(Path::new("/")), "/*");
    }

    /// Native Windows evidence: a canonicalized directory must not leak `\\?\` into a
    /// permission pattern, or the grant cannot match a rule the user wrote as
    /// `C:/dir/*`. Only meaningful on Windows, where `wire_path` strips the prefix.
    #[cfg(windows)]
    #[test]
    fn a_grant_pattern_drops_the_windows_verbatim_prefix() {
        assert_eq!(
            directory_grant_pattern(Path::new(r"\\?\C:\work\shared")),
            "C:/work/shared/*"
        );
        assert_eq!(
            directory_grant_pattern(Path::new(r"\\?\UNC\server\share\dir")),
            "//server/share/dir/*"
        );
    }
}

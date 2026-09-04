//! The single composite `lsp` tool exposed to models.

use crate::{Manager, ManagerError, Position};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zuno_error::ToolError;
use zuno_paths::wire_path;
use zuno_tool::{
    PermissionAsk, Tool, ToolConcurrencyPolicy, ToolContext, ToolEffect, ToolOutput,
    ToolReplayPolicy, TypedTool, erase,
};

const DESCRIPTION: &str = "Interact with language servers for definitions, references, hover information, symbols, implementations, and call hierarchies.";

/// An operation supported by the composite tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum LspOperation {
    GoToDefinition,
    FindReferences,
    Hover,
    DocumentSymbol,
    WorkspaceSymbol,
    GoToImplementation,
    PrepareCallHierarchy,
    IncomingCalls,
    OutgoingCalls,
}

impl LspOperation {
    fn wire_name(self) -> &'static str {
        match self {
            Self::GoToDefinition => "goToDefinition",
            Self::FindReferences => "findReferences",
            Self::Hover => "hover",
            Self::DocumentSymbol => "documentSymbol",
            Self::WorkspaceSymbol => "workspaceSymbol",
            Self::GoToImplementation => "goToImplementation",
            Self::PrepareCallHierarchy => "prepareCallHierarchy",
            Self::IncomingCalls => "incomingCalls",
            Self::OutgoingCalls => "outgoingCalls",
        }
    }
}

/// Parameters shared by every LSP operation.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LspParams {
    /// The LSP operation to perform.
    pub operation: LspOperation,
    /// Absolute path, or a path relative to the tool's workspace directory.
    pub file_path: String,
    /// One-based line number as displayed by editors.
    pub line: u32,
    /// One-based character offset as displayed by editors.
    pub character: u32,
    /// Query used by `workspaceSymbol`; an omitted query requests all symbols.
    #[serde(default)]
    pub query: Option<String>,
}

/// A `filePath` argument placed relative to the tool's roots.
///
/// Resolved once per call so the escalation, the `lsp` ask and the rendered title all
/// describe the same file: what the boundary decision was about and what the user
/// reads cannot disagree.
#[derive(Debug, Clone)]
struct ResolvedTarget {
    /// The path handed to the language server.
    ///
    /// The path that was authorized, not a second string that merely started out the
    /// same: [`LspTool::confirm_unchanged`] re-resolves it immediately before the
    /// handoff and refuses the call unless it still answers `absolute`. Inside a root
    /// it keeps that root's configured spelling rather than its canonical one, because
    /// [`Manager::has_server`] walks root markers with a `starts_with` test against the
    /// workspace the manager was built with, and a canonical prefix (`\\?\C:\ws` on
    /// Windows, `/private/tmp` for a macOS `/tmp` session) stops matching it and takes
    /// every server away. Only the root prefix is respelled; everything below it comes
    /// from the resolved path, so no argument-supplied indirection survives into it.
    open_path: PathBuf,
    /// The file the platform would open for this argument — what the containment
    /// decision is about and what every user-visible rendering shows.
    absolute: PathBuf,
    /// The resource permission rules are matched against: relative to
    /// [`LspTool::anchor_root`] when the target lies under it, the full resolved path
    /// otherwise.
    ///
    /// `read` spells its resource the same way relative to *its* one root
    /// (`FileToolRuntime::resolve` in `zuno-tools/src/read/support.rs`), but that root is
    /// the session directory, while `lsp` — like `glob` and `grep` — treats the worktree
    /// as in boundary too and so has to anchor at the root that covers everything it
    /// admits. For a session opened in a subdirectory the two tools therefore spell the
    /// same file differently, and a rule written for one does not name it for the other.
    /// That divergence is the convergence item the integrator tracks, not something this
    /// crate can settle alone: anchoring at the session directory instead would make
    /// every in-boundary file *outside* that directory fall to an absolute spelling no
    /// repository-relative rule can name.
    ///
    /// Guaranteed by [`spells_one_file`] to name exactly the file in `absolute`, so a
    /// rule that matches it cannot be a rule about some other file.
    resource: String,
    /// The directory an `external_directory` grant has to cover, set only when the
    /// target lies outside both roots.
    external_parent: Option<PathBuf>,
}

/// Composite model-facing LSP tool backed by one workspace manager.
#[derive(Debug, Clone)]
pub struct LspTool {
    manager: Arc<Manager>,
    directory: PathBuf,
    worktree: PathBuf,
    /// The session directory resolved once, at construction — see [`bounding_root`].
    ///
    /// `None` when it bounds nothing: it cannot be resolved, or it is the filesystem
    /// root itself. Such a root stops counting and every target escalates instead,
    /// which is the fail-closed direction. Resolving here rather than per call also
    /// keeps a model-supplied path from spending two extra canonicalization walks over
    /// immutable directories on every request.
    directory_root: Option<PathBuf>,
    /// The worktree root resolved once, on the same terms as `directory_root`.
    ///
    /// Also the anchor every in-boundary permission resource is spelled relative to;
    /// see [`LspTool::anchor_root`].
    worktree_root: Option<PathBuf>,
}

impl LspTool {
    /// Build a tool whose relative paths resolve against `directory`.
    #[must_use]
    pub fn new(
        manager: Arc<Manager>,
        directory: impl Into<PathBuf>,
        worktree: impl Into<PathBuf>,
    ) -> Self {
        let directory = directory.into();
        let worktree = worktree.into();
        Self {
            manager,
            directory_root: bounding_root(&directory),
            worktree_root: bounding_root(&worktree),
            directory,
            worktree,
        }
    }

    /// Erase the typed tool for registration in [`zuno_tool::Tool`].
    #[must_use]
    pub fn erased(self) -> Arc<dyn Tool> {
        erase(self)
    }

    fn resolve_path(&self, value: &str) -> PathBuf {
        let path = Path::new(value);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.directory.join(path)
        }
    }

    /// Places the model's `filePath` relative to the session directory and the worktree.
    ///
    /// Two decisions come out of this, and they are deliberately taken from *different*
    /// values because they answer different questions.
    ///
    /// **Containment** — does this call owe an `external_directory` escalation? — counts
    /// either root, which is how `glob` and `grep` decide it
    /// (`zuno_tools::search_common::SearchScope::contains`): a session opened in a
    /// subdirectory can still reach a sibling directory of the same repository without
    /// an escalation. The comparison is between the file the platform would open and the
    /// resolved roots, so neither a link planted inside the workspace nor a `..` placed
    /// after one can smuggle a file from outside it past the check.
    ///
    /// **The permission resource** — the string permission rules are matched against —
    /// counts exactly ONE root, [`LspTool::anchor_root`], and is derived from `absolute`
    /// alone. Spelling it relative to "whichever of two roots matched first" made the
    /// file -> resource map non-injective: with `directory = <wt>/crates/inner` and
    /// `worktree = <wt>`, the two *different* files `<wt>/crates/inner/src/main.rs` and
    /// `<wt>/src/main.rs` both spelled `src/main.rs`, so an `lsp` allow rule naming one
    /// of them governed the other, and a repo-root-relative deny named neither. A
    /// reduction that lets a resource match a rule that does not name it belongs on the
    /// deny side only, and this one was on the allow side: [`TypedTool::run`] passes the
    /// resource as the ask's only pattern.
    ///
    /// # Errors
    ///
    /// [`ToolError::InvalidArgs`] when the argument names no single file — see
    /// [`resolve_kernel_path`] — or when the resource string that would be matched
    /// against permission rules names more than one file, see [`spells_one_file`].
    /// Refusing is the fail-closed answer: a path the boundary cannot decide about must
    /// not be handed to a language server on the strength of a lexical guess.
    fn resolve_target(&self, value: &str) -> Result<ResolvedTarget, ToolError> {
        let requested = self.resolve_path(value);
        let absolute =
            resolve_kernel_path(&requested).map_err(|source| ToolError::InvalidArgs {
                tool: self.id().to_owned(),
                source: Box::new(std::io::Error::new(
                    source.kind(),
                    format!("filePath {}: {source}", quoted(&wire_path(&requested))),
                )),
            })?;
        // One spelling for every target, inside the boundary or outside it, and a
        // function of `absolute` only — never of whichever root the containment test
        // happened to match first.
        let resource = self.anchored_resource(&absolute)?;
        let contained = [
            (&self.directory, &self.directory_root),
            (&self.worktree, &self.worktree_root),
        ]
        .into_iter()
        .find_map(|(root, resolved)| {
            let relative = absolute.strip_prefix(resolved.as_deref()?).ok()?;
            Some((root, relative.to_path_buf()))
        });
        let Some((root, relative)) = contained else {
            let parent = absolute.parent().unwrap_or(&absolute).to_path_buf();
            return Ok(ResolvedTarget {
                open_path: absolute.clone(),
                resource,
                absolute,
                external_parent: Some(parent),
            });
        };
        // The root's *configured* spelling, not its resolved one, and the only value
        // here that still comes from "whichever root matched": [`Manager::has_server`]
        // walks root markers with a `starts_with` test against the workspace the manager
        // was built with, and a canonical prefix (`\\?\C:\ws` on Windows,
        // `/private/tmp` for a macOS `/tmp` session) stops matching it and takes every
        // server away. Everything below the root comes from the resolved path, and
        // [`LspTool::confirm_unchanged`] re-resolves the result and refuses the call
        // unless it still answers `absolute`, so a wrong prefix cannot become a wrong
        // file.
        let open_path = if relative.as_os_str().is_empty() {
            root.clone()
        } else {
            root.join(&relative)
        };
        Ok(ResolvedTarget {
            open_path,
            absolute,
            resource,
            external_parent: None,
        })
    }

    /// The one root every in-boundary resource string is spelled relative to.
    ///
    /// The worktree, because in every layout the harness can produce it *contains* the
    /// session directory: `zuno_paths::project::worktree_root` derives it from
    /// `git rev-parse --show-toplevel` starting at the session's own working directory,
    /// so it is an ancestor of that directory or equal to it. Anchoring there means a
    /// rule is spelled from the repository root — the same place `glob` renders its
    /// title from — and, because one fixed root cannot describe two files with one
    /// string, it is what makes the resource injective.
    ///
    /// Falls back to the session directory when the worktree bounds nothing, and is
    /// `None` when neither root does, in which case every resource is the full resolved
    /// path. In the layouts the harness produces, "under either root" and "under the
    /// anchor" are therefore the same set, so the containment decision and the resource
    /// spelling cannot disagree; when a caller supplies roots that are not nested,
    /// a target contained by the *other* root falls to the absolute spelling, which is
    /// the most specific one available and still injective.
    fn anchor_root(&self) -> Option<&Path> {
        self.worktree_root
            .as_deref()
            .or(self.directory_root.as_deref())
    }

    /// The permission resource for `absolute`: anchor-relative when it lies under
    /// [`LspTool::anchor_root`], the full resolved path when it does not.
    ///
    /// Injective on every platform, which is the property the two-root spelling lacked.
    /// Inside the anchor-relative branch, `strip_prefix` against one *fixed* prefix is
    /// injective; inside the absolute branch, identity is. The branches cannot collide
    /// either: a relative path has no root component, so on POSIX its wire spelling
    /// never begins with `/`, and on Windows it can never begin with a drive or UNC
    /// prefix because `:` is not legal in a component name. `.` is reached only for the
    /// anchor root itself and is a fixed literal rather than a reduction of the empty
    /// relative path.
    ///
    /// # Errors
    ///
    /// [`ToolError::InvalidArgs`] when [`spells_one_file`] finds the reduced spelling
    /// names more than the one file.
    fn anchored_resource(&self, absolute: &Path) -> Result<String, ToolError> {
        let anchored = self
            .anchor_root()
            .and_then(|anchor| absolute.strip_prefix(anchor).ok());
        let native = match anchored {
            Some(relative) if relative.as_os_str().is_empty() => return Ok(".".to_owned()),
            Some(relative) => relative,
            None => absolute,
        };
        let resource = wire_path(native);
        self.require_one_file(&resource, native)?;
        Ok(resource)
    }

    /// Refuses the call when `resource` would be matched on behalf of more than one file.
    ///
    /// # Errors
    ///
    /// [`ToolError::InvalidArgs`], with the spelling the reduction merged, so the user
    /// can see why the file cannot be named.
    fn require_one_file(&self, resource: &str, native: &Path) -> Result<(), ToolError> {
        if spells_one_file(resource, native) {
            return Ok(());
        }
        Err(ToolError::InvalidArgs {
            tool: self.id().to_owned(),
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "filePath {}: is not statically resolvable as a permission resource, \
                     because {} names it too",
                    native.display(),
                    resource
                ),
            )),
        })
    }

    /// Refuses the call unless the path about to be handed over still resolves to the
    /// path that was authorized.
    ///
    /// The `lsp` and `external_directory` asks can wait on a human for minutes. In that
    /// window a concurrent `shell` job, a second agent, or an earlier tool call of this
    /// same session can turn a component of the authorized path into a link, and an
    /// out-of-process language server resolves the path itself — there is no anchored
    /// descriptor to hand it the way `read` opens a file beneath its authority root. A
    /// second resolution here narrows the window to the syscalls between this
    /// comparison and the server's own open, and a mismatch fails the call instead of
    /// opening a file the escalation never named.
    ///
    /// # Errors
    ///
    /// [`ToolError::Failed`] when the path moved or stopped resolving.
    fn confirm_unchanged(&self, target: &ResolvedTarget) -> Result<(), ToolError> {
        let failure = |detail: String| ToolError::Failed {
            tool: self.id().to_owned(),
            source: Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, detail)),
        };
        let current = resolve_kernel_path(&target.open_path).map_err(|source| {
            failure(format!(
                "{} stopped resolving after it was authorized: {source}",
                wire_path(&target.open_path)
            ))
        })?;
        if current != target.absolute {
            return Err(failure(format!(
                "{} now resolves to {}, not to the authorized {}",
                wire_path(&target.open_path),
                wire_path(&current),
                wire_path(&target.absolute)
            )));
        }
        Ok(())
    }

    /// Raises the `external_directory` escalation for a target outside both roots.
    ///
    /// A language server reads the file it is handed and returns content-derived text
    /// — hover strings, symbol names, diagnostics — to the model, so `lsp` owes the
    /// same escalation every other file-reading tool performs. The permission key, the
    /// pattern spelling and the metadata keys are the ones `read`, `glob` and `grep`
    /// use, so one standing decision covers all four for the same directory.
    async fn assert_external_directory(
        &self,
        ctx: &ToolContext,
        target: &ResolvedTarget,
    ) -> Result<(), ToolError> {
        let Some(parent) = target.external_parent.as_deref() else {
            return Ok(());
        };
        // Byte-identical to `zuno_tools::search_common::directory_grant_pattern`, so one
        // standing decision still covers `lsp`, `read`, `glob` and `grep` for the same
        // directory. The pattern needs no reduction audit of its own: `resolve_target`
        // has already refused any target whose own resource spelling was ambiguous, and
        // this directory is a prefix of that path.
        let pattern = wire_path(&parent.join("*"));
        let patterns = vec![pattern.clone()];
        let mut metadata = Map::new();
        metadata.insert(
            "filepath".to_owned(),
            Value::String(wire_path(&target.absolute)),
        );
        metadata.insert("parentDir".to_owned(), Value::String(wire_path(parent)));
        ctx.ask(
            self.id(),
            PermissionAsk {
                permission: "external_directory".to_owned(),
                patterns,
                metadata,
                always: vec![pattern],
                ..PermissionAsk::default()
            },
        )
        .await
    }

    /// The rendered heading for a completed call.
    ///
    /// Spelled relative to [`LspTool::anchor_root`], the same root the permission
    /// resource is anchored at, so the string a user reads in the transcript is the
    /// string a rule about that file has to name. Reading `worktree_root` directly here
    /// was a second copy of the "which root" decision: when the worktree bounds nothing
    /// but the session directory does, the resource was directory-relative while the
    /// title stayed absolute.
    fn title(
        &self,
        operation: LspOperation,
        target: &ResolvedTarget,
        line: u32,
        character: u32,
    ) -> String {
        if operation == LspOperation::WorkspaceSymbol {
            return operation.wire_name().to_owned();
        }
        let absolute = target.absolute.as_path();
        let relative = wire_path(
            self.anchor_root()
                .and_then(|root| absolute.strip_prefix(root).ok())
                .unwrap_or(absolute),
        );
        if operation == LspOperation::DocumentSymbol {
            format!("{} {relative}", operation.wire_name())
        } else {
            format!("{} {relative}:{line}:{character}", operation.wire_name())
        }
    }
}

/// Longest `filePath` this tool will try to resolve, in bytes.
///
/// The Windows extended-length ceiling — 32,767 characters — is the longest path any
/// supported platform can name; Linux's `PATH_MAX` is 4096 and macOS's is 1024.
///
/// The comparison is in UTF-8 bytes and Windows counts UTF-16 code units, so a path made
/// of non-ASCII characters could in principle be under Windows' ceiling and over this one.
/// That direction is the safe one — it over-refuses a path no real tree contains rather
/// than admitting an unbounded walk — and it is named here rather than left for the next
/// reader to notice. A longer
/// argument cannot be a file on any of them, so refusing it costs nothing real and makes
/// the refusal the same everywhere instead of depending on whether the kernel answers
/// `ENAMETOOLONG` or — as it does whenever the first missing component comes early —
/// `ENOENT`. Checked before the component count so that count is itself bounded: counting
/// the components of a 2 MB argument measured 5.2 ms on its own.
const MAX_PATH_BYTES: usize = 32_767;

/// Most components this tool will try to resolve for one `filePath`.
///
/// [`resolve_kernel_path`] pays one [`Path::canonicalize`] per missing component and each
/// call re-passes a shrinking path, so the walk is quadratic in the component count.
/// Measured on Linux x86_64 with the extracted function against a real fixture: 1,000
/// components cost 1.59 ms, 20,000 cost 59.4 ms, 100,000 cost 947 ms. `run` pays that
/// twice — once in [`LspTool::resolve_target`] and once in
/// [`LspTool::confirm_unchanged`] — the tool declares
/// [`ToolConcurrencyPolicy::ParallelSafe`], and none of it is behind a prompt, so a
/// 200 KB `filePath` bought a model roughly a second of a runtime worker per call, in
/// parallel. At this bound the same walk measures 1.62 ms, and no real path is 1,024
/// directories deep.
const MAX_PATH_COMPONENTS: usize = 1_024;

/// A model-supplied string as it will be quoted back in a refusal, truncated.
///
/// Without this, refusing an oversized `filePath` echoed the whole argument into a
/// durable tool error, so the cheapest way to put megabytes into the transcript was to
/// send megabytes and be told no.
fn quoted(text: &str) -> String {
    const LIMIT: usize = 256;
    match text.char_indices().nth(LIMIT) {
        Some((cut, _)) => format!("{}… ({} bytes)", &text[..cut], text.len()),
        None => text.to_owned(),
    }
}

/// The resolved form of a root that can actually bound something, or `None`.
///
/// A root is unusable when it cannot be resolved — it does not exist, or an ancestor
/// cannot be read — and *also* when it is the filesystem root itself. Containment against
/// `/` (or `C:\`) is vacuous: every absolute path on the machine lies under it, so `lsp`
/// would raise no `external_directory` escalation for anything, anywhere, and the resource
/// for `/etc/shadow` would be the rootless `etc/shadow`. `zuno_paths::resolve_project`
/// reports exactly that root for a session outside any repository, which is the wiring a
/// caller most easily lands on. Dropping such a root costs one extra escalation per
/// directory and is the fail-closed direction.
fn bounding_root(path: &Path) -> Option<PathBuf> {
    let resolved = resolve_kernel_path(path).ok()?;
    resolved.parent().is_some().then_some(resolved)
}

/// Resolves `path` to the file the platform would open, or refuses to guess.
///
/// Links are resolved as far down the path as it exists and the missing tail is
/// re-appended, which is how `read` decides a path that does not exist yet
/// (`canonicalize_allow_missing` in `zuno-tools/src/read/support.rs`). Resolving the
/// longest existing prefix rather than requiring the whole path to exist also keeps a
/// workspace reached through a link — macOS `/tmp` — from reading as external, and
/// avoids a spurious prompt on a file that is about to be created.
///
/// `..` is deliberately **not** folded lexically first, which is what the previous
/// version got wrong. The two platforms disagree about what `..` means and only the
/// platform can settle it:
///
/// * POSIX applies `..` to the directory a link resolved to, so `<ws>/link/../x` reads
///   *outside* the workspace when `link` points outside it.
/// * The Win32 path layer folds `..` before the object manager sees a non-verbatim
///   path, so the same spelling reads `<ws>/x` even when `link` is a junction.
///
/// Popping components off the **raw** path and letting [`Path::canonicalize`] answer
/// delegates the rule to the platform on both. Folding first agreed with Windows and
/// silently disagreed with POSIX, which is how a `..` placed immediately after a
/// workspace link decided about a file inside the workspace while the server opened one
/// outside it.
///
/// # Errors
///
/// When the argument is past [`MAX_PATH_BYTES`] or [`MAX_PATH_COMPONENTS`], and when it
/// names no single file. Only a missing component is popped past; a `..` the platform will
/// not resolve — an ancestor is missing on POSIX, or a verbatim `\\?\` path, where Windows
/// passes `..` through literally — and an ancestor that cannot be read both fail here. No
/// spelling of such a path is one a boundary decision could be about, so the caller
/// refuses rather than falling back to a lexical guess that could read as contained.
fn resolve_kernel_path(path: &Path) -> std::io::Result<PathBuf> {
    let bytes = path.as_os_str().len();
    if bytes > MAX_PATH_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "is {bytes} bytes long, past the {MAX_PATH_BYTES}-byte ceiling of every \
                 supported platform, so it names no file"
            ),
        ));
    }
    let components = path.components().count();
    if components > MAX_PATH_COMPONENTS {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "has {components} path components, past the {MAX_PATH_COMPONENTS} this tool \
                 resolves; resolving it would cost one filesystem walk per component, twice, \
                 before any permission is asked"
            ),
        ));
    }
    let mut head = path.to_path_buf();
    let mut tail: Vec<OsString> = Vec::new();
    loop {
        match head.canonicalize() {
            Ok(mut resolved) => {
                for part in tail.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // `Path::file_name` is `None` exactly when the path ends in `..` or has
                // no component left, which is where the fail-closed exit lives: there is
                // no existing directory for the platform to apply that `..` to.
                let Some(name) = head.file_name().map(OsString::from) else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!(
                            "is not statically resolvable: nothing above {} exists, so `..` in it has no answer",
                            wire_path(&head)
                        ),
                    ));
                };
                tail.push(name);
                if !head.pop() {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

/// Whether `reduced` names `native` and no other file.
///
/// Two reductions stand between a path and the string permission rules are matched
/// against. [`wire_path`] maps `\` to `/`, and a lossy conversion maps every byte that is
/// not valid Unicode to `U+FFFD`.
///
/// On Windows both reductions keep identity for the first one and not the second: `\` and
/// `/` are the same separator and `\` cannot occur in a Windows file name, and the
/// `\\?\` prefix `wire_path` strips is a syntax marker rather than part of the name — but
/// an unpaired surrogate in a UTF-16 name still has no faithful `str`. On Linux and macOS
/// `\` is an ordinary byte, so `a\b.rs` and `a/b.rs` are two different files that reduce
/// to one resource string.
///
/// A reduction that can make one file match a rule naming another belongs on the deny
/// side only, and this one cannot be pushed there from here: carrying the unreduced
/// spelling as an extra pattern does nothing, because `zuno_permission::wildcard::
/// wildcard_match` applies the same `\` merge to the rule's pattern *and* to every
/// spelling it is offered, on every platform — verified against that function's source,
/// where `wildcard_match("a\\b.rs", "a/b.rs")` is `true`. The only fail-closed answer
/// available to this crate is to refuse a path it cannot name, which over-refuses an
/// unusual file name rather than acting on an allow rule that names a different file.
fn spells_one_file(reduced: &str, native: &Path) -> bool {
    spells_one_file_under(reduced, native, NameRules::current())
}

/// Which platform's file-name rules a reduced resource spelling is judged by.
///
/// A parameter rather than a `cfg!`, the way `zuno_permission::Spellings::for_host` takes
/// its host: the two arms are *designed* to answer differently for the same bytes, and a
/// `cfg!` leaves whichever arm is not this host's untestable everywhere the tests actually
/// run. With it as a parameter, the Windows claim — that the arm must not refuse the
/// verbatim `\\?\C:\...` form [`Path::canonicalize`] returns there — is checked on Linux
/// too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NameRules {
    /// `\` separates components and cannot occur inside a component name, and the `\\?\`
    /// prefix [`wire_path`] strips is a syntax marker rather than part of the name. The
    /// only reduction left to catch is a UTF-16 name with no faithful `str`.
    Windows,
    /// `\` is an ordinary byte inside a component name, so `a\b.rs` and `a/b.rs` are two
    /// different files whose wire spellings collide.
    Posix,
}

impl NameRules {
    /// The rules of the host this build runs on.
    fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Posix
        }
    }
}

/// [`spells_one_file`] under an explicitly named platform's name rules.
fn spells_one_file_under(reduced: &str, native: &Path, rules: NameRules) -> bool {
    match rules {
        NameRules::Windows => native.to_str().is_some(),
        NameRules::Posix => native.to_str() == Some(reduced),
    }
}

#[async_trait]
impl TypedTool for LspTool {
    type Params = LspParams;

    fn id(&self) -> &str {
        "lsp"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Safe
    }

    fn concurrency_policy(&self) -> ToolConcurrencyPolicy {
        ToolConcurrencyPolicy::ParallelSafe
    }

    fn effect(&self, _args: &Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    async fn run(&self, params: LspParams, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        if params.line == 0 || params.character == 0 {
            return Err(ToolError::InvalidArgs {
                tool: self.id().to_owned(),
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "line and character must be one-based positive integers",
                )),
            });
        }

        let target = self.resolve_target(&params.file_path)?;
        // Escalated before the `lsp` grant is consulted, the order `read` uses: a
        // standing `lsp` decision must not by itself reach a file the user never
        // agreed to leave the workspace for, and a refused escalation should not have
        // already spent an `lsp` prompt. Every operation opens the file on the server,
        // `workspaceSymbol` included, so the check is unconditional.
        self.assert_external_directory(&ctx, &target).await?;

        let mut metadata = Map::new();
        metadata.insert(
            "operation".to_owned(),
            Value::String(params.operation.wire_name().to_owned()),
        );
        if params.operation != LspOperation::WorkspaceSymbol {
            // `filepath`, the key `read`, `edit`, `write` and the search tools already
            // write and the only one the TUI's metadata fallback reads. The camelCase
            // spelling this tool used was invisible to every consumer.
            metadata.insert(
                "filepath".to_owned(),
                Value::String(wire_path(&target.absolute)),
            );
        }
        if !matches!(
            params.operation,
            LspOperation::WorkspaceSymbol | LspOperation::DocumentSymbol
        ) {
            metadata.insert("line".to_owned(), Value::from(params.line));
            metadata.insert("character".to_owned(), Value::from(params.character));
        }
        ctx.ask(
            self.id(),
            PermissionAsk {
                permission: "lsp".to_owned(),
                // The resource, not `"*"`. `lsp` is not an action-only key, so a
                // config author may scope it per pattern; a wildcard here made every
                // such rule unmatchable. `always` still offers the whole tool, as it
                // does for `read`, `glob` and `grep` — the workspace boundary is held
                // by the escalation above, not by narrowing this offer.
                patterns: vec![target.resource.clone()],
                metadata,
                always: vec!["*".to_owned()],
                ..PermissionAsk::default()
            },
        )
        .await?;

        // Nothing between the decision and the handoff may have moved the file the two
        // asks named. Checked after the asks, because the ask is the long wait.
        self.confirm_unchanged(&target)?;

        let file = target.open_path.as_path();
        if !tokio::fs::try_exists(file)
            .await
            .map_err(|source| ToolError::Failed {
                tool: self.id().to_owned(),
                source: Box::new(source),
            })?
        {
            return Err(ToolError::Failed {
                tool: self.id().to_owned(),
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("file not found: {}", wire_path(&target.absolute)),
                )),
            });
        }
        if !self.manager.has_server(file) {
            return Err(ToolError::Failed {
                tool: self.id().to_owned(),
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no LSP server available for this file type",
                )),
            });
        }
        self.manager.touch_file(file).await.map_err(tool_failure)?;

        let position = Position {
            line: params.line - 1,
            character: params.character - 1,
        };
        let result = match params.operation {
            LspOperation::GoToDefinition => {
                self.manager
                    .position_request(file, position, "textDocument/definition", json!({}))
                    .await
            }
            LspOperation::FindReferences => {
                self.manager
                    .position_request(
                        file,
                        position,
                        "textDocument/references",
                        json!({ "context": { "includeDeclaration": true } }),
                    )
                    .await
            }
            LspOperation::Hover => {
                self.manager
                    .position_request(file, position, "textDocument/hover", json!({}))
                    .await
            }
            LspOperation::DocumentSymbol => self.manager.document_symbols(file).await,
            LspOperation::WorkspaceSymbol => {
                self.manager
                    .workspace_symbols(params.query.as_deref().unwrap_or_default())
                    .await
            }
            LspOperation::GoToImplementation => {
                self.manager
                    .position_request(file, position, "textDocument/implementation", json!({}))
                    .await
            }
            LspOperation::PrepareCallHierarchy => {
                self.manager
                    .position_request(
                        file,
                        position,
                        "textDocument/prepareCallHierarchy",
                        json!({}),
                    )
                    .await
            }
            LspOperation::IncomingCalls => {
                self.manager
                    .call_hierarchy(file, position, "callHierarchy/incomingCalls")
                    .await
            }
            LspOperation::OutgoingCalls => {
                self.manager
                    .call_hierarchy(file, position, "callHierarchy/outgoingCalls")
                    .await
            }
        }
        .map_err(tool_failure)?;

        let output = if result.is_empty() {
            format!("No results found for {}", params.operation.wire_name())
        } else {
            serde_json::to_string_pretty(&result).map_err(|source| ToolError::Failed {
                tool: self.id().to_owned(),
                source: Box::new(source),
            })?
        };
        Ok(ToolOutput::text(
            self.title(params.operation, &target, params.line, params.character),
            output,
        )
        .with_metadata("result", Value::Array(result)))
    }
}

fn tool_failure(source: ManagerError) -> ToolError {
    ToolError::Failed {
        tool: "lsp".to_owned(),
        source: Box::new(source),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use zuno_tool::{
        AllowAll, INTENT_KEY, NeverInterrupted, PermissionAsker, PermissionOrigin, ToolContext,
    };

    fn tool(directory: &Path, worktree: &Path) -> LspTool {
        let config = zuno_config::schema::lsp::LspConfig::Enabled(false);
        let registry = Arc::new(crate::ServerRegistry::offline(
            &zuno_catalog::lsp_config::ResolvedLsp::resolve(Some(&config)),
        ));
        let manager = Arc::new(Manager::new(
            directory,
            registry,
            crate::RestartPolicy::default(),
            std::num::NonZeroUsize::new(4).expect("non-zero"),
        ));
        LspTool::new(manager, directory, worktree)
    }

    /// The resolved form of a fixture path, which is what every prompt renders.
    fn resolved(path: &Path) -> PathBuf {
        resolve_kernel_path(path).expect("a fixture path resolves")
    }

    /// Records every ask in order, then admits it, so a test can read the sequence the
    /// real `run` produced rather than a reimplementation of it.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<PermissionAsk>>);

    impl Recorder {
        fn keys(&self) -> Vec<String> {
            self.asks()
                .into_iter()
                .map(|ask| ask.permission)
                .collect::<Vec<_>>()
        }

        fn asks(&self) -> Vec<PermissionAsk> {
            self.0.lock().expect("recorded asks").clone()
        }
    }

    #[async_trait]
    impl PermissionAsker for Recorder {
        async fn ask(
            &self,
            _origin: PermissionOrigin<'_>,
            _tool: &str,
            ask: PermissionAsk,
        ) -> Result<(), ToolError> {
            self.0.lock().expect("recorded asks").push(ask);
            Ok(())
        }
    }

    /// Records asks and refuses the `external_directory` escalation, so a test can
    /// prove the escalation actually gates the call rather than merely being emitted.
    #[derive(Default)]
    struct DenyExternal(Mutex<Vec<String>>);

    #[async_trait]
    impl PermissionAsker for DenyExternal {
        async fn ask(
            &self,
            _origin: PermissionOrigin<'_>,
            tool: &str,
            ask: PermissionAsk,
        ) -> Result<(), ToolError> {
            self.0
                .lock()
                .expect("recorded keys")
                .push(ask.permission.clone());
            if ask.permission == "external_directory" {
                return Err(ToolError::Denied {
                    tool: tool.to_owned(),
                });
            }
            Ok(())
        }
    }

    /// A tool whose manager really would open the file, so "`touch_file` was never
    /// reached" is an observation rather than a tautology.
    ///
    /// The [`tool`] helper builds a manager with LSP switched off, where `has_server` is
    /// always `false` and nothing downstream of it can run at all. Here one custom server
    /// claims the file *name* `main.rs` with a command that resolves nowhere: `has_server`
    /// answers `true`, and reaching `touch_file` registers a supervisor that
    /// [`Manager::status`] reports. No built-in competes for it — the `rust` built-in
    /// claims `.rs` but its `RustWorkspace` root policy finds no `Cargo.toml` in a bare
    /// fixture and yields no root.
    fn tool_with_a_matching_server(directory: &Path, worktree: &Path) -> (Arc<Manager>, LspTool) {
        let config: zuno_config::schema::lsp::LspConfig = serde_json::from_value(json!({
            "zuno-lsp-handoff-probe": {
                "command": ["zuno-lsp-handoff-probe-not-on-path"],
                "extensions": ["main.rs"]
            }
        }))
        .expect("a custom server entry");
        let registry = Arc::new(crate::ServerRegistry::offline(
            &zuno_catalog::lsp_config::ResolvedLsp::resolve(Some(&config)),
        ));
        let manager = Arc::new(Manager::new(
            directory,
            registry,
            crate::RestartPolicy {
                maximum_restarts: 0,
                ..crate::RestartPolicy::default()
            },
            std::num::NonZeroUsize::new(4).expect("non-zero"),
        ));
        let tool = LspTool::new(Arc::clone(&manager), directory, worktree);
        (manager, tool)
    }

    /// Replaces the authorized directory with a link out of the workspace *from inside the
    /// prompt*, which is where the window actually is: the ask can wait on a human while a
    /// concurrent `shell` job or a second agent moves a component of the path.
    #[cfg(unix)]
    struct SwapOnAsk {
        workspace: PathBuf,
        outside: PathBuf,
        asked: Mutex<Vec<String>>,
    }

    #[cfg(unix)]
    impl SwapOnAsk {
        fn keys(&self) -> Vec<String> {
            self.asked.lock().expect("recorded keys").clone()
        }
    }

    #[cfg(unix)]
    #[async_trait]
    impl PermissionAsker for SwapOnAsk {
        async fn ask(
            &self,
            _origin: PermissionOrigin<'_>,
            _tool: &str,
            ask: PermissionAsk,
        ) -> Result<(), ToolError> {
            self.asked
                .lock()
                .expect("recorded keys")
                .push(ask.permission);
            std::fs::remove_dir_all(self.workspace.join("src")).expect("remove the real directory");
            std::os::unix::fs::symlink(&self.outside, self.workspace.join("src"))
                .expect("plant the link");
            Ok(())
        }
    }

    fn context(permission: Arc<dyn PermissionAsker>) -> ToolContext {
        ToolContext::new(
            "session",
            "message",
            "call",
            "build",
            permission,
            Arc::new(NeverInterrupted),
        )
    }

    fn params(file_path: &str) -> LspParams {
        LspParams {
            operation: LspOperation::Hover,
            file_path: file_path.to_owned(),
            line: 1,
            character: 1,
            query: None,
        }
    }

    #[test]
    fn composite_schema_uses_upstream_operation_and_field_names() {
        let tool = tool(Path::new("/tmp"), Path::new("/tmp")).erased();
        let definition = tool.definition();
        assert_eq!(definition.id, "lsp");
        assert_eq!(tool.replay_policy(), ToolReplayPolicy::Safe);
        assert_eq!(
            definition.parameters["properties"]["filePath"]["type"],
            "string"
        );
        let operations = definition.parameters["properties"]["operation"]["enum"]
            .as_array()
            .expect("operation enum");
        assert!(operations.contains(&json!("goToDefinition")));
        assert!(operations.contains(&json!("outgoingCalls")));
        assert_eq!(
            definition.parameters["properties"][INTENT_KEY]["type"],
            "string"
        );
        assert!(
            !definition.parameters["required"]
                .as_array()
                .expect("required fields")
                .contains(&json!(INTENT_KEY)),
            "intent is optional metadata for LSP just as it is for every tool"
        );
    }

    #[tokio::test]
    async fn a_target_outside_the_roots_escalates_before_the_lsp_grant_is_consulted() {
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let secret = outside.path().join("config");
        std::fs::write(&secret, "IdentityFile ~/.ssh/id_ed25519\n").expect("write secret");

        let recorder = Arc::new(Recorder::default());
        let tool = tool(workspace.path(), workspace.path());
        // The call fails afterwards for want of a language server; the asks it raised
        // first are the boundary under test.
        let _ = tool
            .run(
                params(&secret.to_string_lossy()),
                context(Arc::clone(&recorder) as Arc<dyn PermissionAsker>),
            )
            .await;

        assert_eq!(
            recorder.keys(),
            vec!["external_directory".to_owned(), "lsp".to_owned()],
            "an out-of-workspace LSP target must escalate the way `read` does"
        );
        let asks = recorder.asks();
        let expected = wire_path(&resolved(outside.path()).join("*"));
        assert_eq!(asks[0].patterns, vec![expected.clone()]);
        assert_eq!(asks[0].always, vec![expected]);
        assert_eq!(
            asks[0].metadata["parentDir"],
            Value::String(wire_path(&resolved(outside.path())))
        );
    }

    #[tokio::test]
    async fn a_refused_external_directory_escalation_stops_the_call() {
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let secret = outside.path().join("config");
        std::fs::write(&secret, "IdentityFile ~/.ssh/id_ed25519\n").expect("write secret");

        let asker = Arc::new(DenyExternal::default());
        let tool = tool(workspace.path(), workspace.path());
        let error = tool
            .run(
                params(&secret.to_string_lossy()),
                context(Arc::clone(&asker) as Arc<dyn PermissionAsker>),
            )
            .await
            .expect_err("a refused escalation must deny the call");

        assert!(
            matches!(error, ToolError::Denied { ref tool } if tool == "lsp"),
            "expected a denial, got {error:?}"
        );
        assert_eq!(
            *asker.0.lock().expect("recorded keys"),
            vec!["external_directory".to_owned()],
            "nothing past the refused escalation may be asked for, let alone opened"
        );
    }

    #[tokio::test]
    async fn a_workspace_target_asks_lsp_for_its_resource_and_never_escalates() {
        let workspace = tempfile::tempdir().expect("workspace");
        let nested = workspace.path().join("src");
        std::fs::create_dir_all(&nested).expect("create nested");
        let file = nested.join("main.rs");
        std::fs::write(&file, "fn main() {}\n").expect("write file");

        let recorder = Arc::new(Recorder::default());
        let tool = tool(workspace.path(), workspace.path());
        let _ = tool
            .run(
                params("src/main.rs"),
                context(Arc::clone(&recorder) as Arc<dyn PermissionAsker>),
            )
            .await;

        assert_eq!(recorder.keys(), vec!["lsp".to_owned()]);
        let asks = recorder.asks();
        assert_eq!(
            asks[0].patterns,
            vec!["src/main.rs".to_owned()],
            "a wildcard pattern makes every per-pattern `lsp` config rule unmatchable"
        );
        assert_eq!(asks[0].always, vec!["*".to_owned()]);
        assert_eq!(
            asks[0].metadata["filepath"],
            Value::String(wire_path(&resolved(&file))),
            "the prompt shows the wire path every other file tool shows"
        );
    }

    #[tokio::test]
    async fn a_parent_traversal_out_of_the_workspace_escalates_on_where_it_lands() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("project");
        let sibling = root.path().join("secrets");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        std::fs::create_dir_all(&sibling).expect("create sibling");
        std::fs::write(sibling.join("token"), "shhh\n").expect("write token");

        let recorder = Arc::new(Recorder::default());
        let tool = tool(&workspace, &workspace);
        let _ = tool
            .run(
                params("../secrets/token"),
                context(Arc::clone(&recorder) as Arc<dyn PermissionAsker>),
            )
            .await;

        assert_eq!(
            recorder.keys(),
            vec!["external_directory".to_owned(), "lsp".to_owned()],
            "containment is decided on where `..` lands, not on the literal spelling"
        );
        let asks = recorder.asks();
        assert_eq!(
            asks[0].patterns,
            vec![wire_path(&resolved(&sibling).join("*"))]
        );
        // Both prompts name where the path lands, not its literal spelling, so the
        // user cannot approve `project/` and be shown a file in `secrets/`.
        let landed = Value::String(wire_path(&resolved(&sibling).join("token")));
        assert_eq!(asks[0].metadata["filepath"], landed);
        assert_eq!(asks[1].metadata["filepath"], landed);
        assert_eq!(
            asks[1].patterns,
            vec![wire_path(&resolved(&sibling).join("token"))]
        );
    }

    #[tokio::test]
    async fn a_sibling_of_the_worktree_root_stays_inside_the_boundary() {
        let worktree = tempfile::tempdir().expect("worktree");
        let directory = worktree.path().join("crates/inner");
        std::fs::create_dir_all(&directory).expect("create directory");
        let sibling = worktree.path().join("crates/other");
        std::fs::create_dir_all(&sibling).expect("create sibling");
        std::fs::write(sibling.join("lib.rs"), "pub fn f() {}\n").expect("write sibling file");

        let recorder = Arc::new(Recorder::default());
        let tool = tool(&directory, worktree.path());
        let _ = tool
            .run(
                params(&sibling.join("lib.rs").to_string_lossy()),
                context(Arc::clone(&recorder) as Arc<dyn PermissionAsker>),
            )
            .await;

        assert_eq!(
            recorder.keys(),
            vec!["lsp".to_owned()],
            "the worktree root counts as a root, as it does for `glob` and `grep`"
        );
        assert_eq!(
            recorder.asks()[0].patterns,
            vec!["crates/other/lib.rs".to_owned()]
        );
    }

    /// The verifier's fixture: two different files, one resource string.
    ///
    /// With `directory = <wt>/crates/inner` and `worktree = <wt>` the resource used to be
    /// spelled relative to whichever root matched first, so `<dir>/src/main.rs` and
    /// `<wt>/src/main.rs` — two files, different bytes — both arrived at the permission
    /// engine as `src/main.rs`. An exact `lsp` allow rule written for the repo-root file
    /// then governed the inner-crate file, and a repo-root-relative deny named neither.
    #[tokio::test]
    async fn two_different_files_named_src_main_rs_get_two_permission_resources() {
        let worktree = tempfile::tempdir().expect("worktree");
        let directory = worktree.path().join("crates/inner");
        std::fs::create_dir_all(directory.join("src")).expect("create the inner crate");
        std::fs::create_dir_all(worktree.path().join("src")).expect("create the repo src");
        let inner = directory.join("src/main.rs");
        let outer = worktree.path().join("src/main.rs");
        std::fs::write(&inner, "// INNER\n").expect("write the inner file");
        std::fs::write(&outer, "// OUTER\n").expect("write the repo-root file");
        // The premise, asserted rather than assumed: two files, not one.
        assert_ne!(
            std::fs::read_to_string(&inner).expect("read inner"),
            std::fs::read_to_string(&outer).expect("read outer")
        );

        let tool = tool(&directory, worktree.path());
        let inner_recorder = Arc::new(Recorder::default());
        let _ = tool
            .run(
                params("src/main.rs"),
                context(Arc::clone(&inner_recorder) as Arc<dyn PermissionAsker>),
            )
            .await;
        let outer_recorder = Arc::new(Recorder::default());
        let _ = tool
            .run(
                params(&outer.to_string_lossy()),
                context(Arc::clone(&outer_recorder) as Arc<dyn PermissionAsker>),
            )
            .await;

        assert_eq!(inner_recorder.keys(), vec!["lsp".to_owned()]);
        assert_eq!(outer_recorder.keys(), vec!["lsp".to_owned()]);
        let inner_patterns = inner_recorder.asks()[0].patterns.clone();
        let outer_patterns = outer_recorder.asks()[0].patterns.clone();
        assert_ne!(
            inner_patterns, outer_patterns,
            "two different files must not be matched against permission rules under one \
             resource string"
        );
        assert_eq!(outer_patterns, vec!["src/main.rs".to_owned()]);
        assert_eq!(inner_patterns, vec!["crates/inner/src/main.rs".to_owned()]);
    }

    /// Containment counts either root; the resource is anchored at exactly one. This
    /// pins what happens in the one layout where those two sets cannot coincide.
    ///
    /// No wiring in the tree produces it — `zuno_paths::project::worktree_root` derives
    /// the worktree by walking up from the session directory, and `LspTool::new` has no
    /// caller outside this crate — but a future caller could pass unrelated roots, so the
    /// direction of the divergence is asserted rather than argued: the target is still in
    /// boundary (no `external_directory` escalation, because the session directory bounds
    /// it, exactly as `SearchScope::contains` decides it for `glob` and `grep`) while its
    /// resource falls back to the full resolved path. That is the most specific spelling
    /// available and still injective, so it can only cost an extra prompt — a relative
    /// allow rule stops matching — and can never let a rule about one file govern
    /// another. The title follows the resource, not the other root.
    #[tokio::test]
    async fn roots_that_are_not_nested_stay_in_boundary_but_are_named_absolutely() {
        let base = tempfile::tempdir().expect("base");
        let alpha = base.path().join("alpha");
        let beta = base.path().join("beta");
        std::fs::create_dir_all(alpha.join("src")).expect("create the session directory");
        std::fs::create_dir_all(&beta).expect("create the unrelated worktree");
        std::fs::write(alpha.join("src/main.rs"), "fn main() {}\n").expect("write main.rs");

        let recorder = Arc::new(Recorder::default());
        let tool = tool(&alpha, &beta);
        let _ = tool
            .run(
                params("src/main.rs"),
                context(Arc::clone(&recorder) as Arc<dyn PermissionAsker>),
            )
            .await;

        assert_eq!(
            recorder.keys(),
            vec!["lsp".to_owned()],
            "the session directory still bounds its own files"
        );
        let absolute = wire_path(&resolved(&alpha.join("src/main.rs")));
        assert_eq!(
            recorder.asks()[0].patterns,
            vec![absolute.clone()],
            "a target the anchor cannot spell is named by its full resolved path"
        );
        let target = tool
            .resolve_target("src/main.rs")
            .expect("the fixture resolves");
        assert_eq!(
            tool.title(LspOperation::Hover, &target, 1, 1),
            format!("hover {absolute}:1:1"),
            "the rendered spelling has to be the spelling a rule must name"
        );
    }

    /// The positive control for the "`touch_file` was never reached" assertion in
    /// [`run_refuses_a_path_swapped_while_the_permission_prompt_was_open`].
    ///
    /// On the same fixture, with the same manager and nothing swapped, `run` walks past
    /// the handoff check and *does* reach `touch_file` — the probe server is registered
    /// and then fails to start, which is a different error from the swap refusal. Without
    /// this, `manager.status().await.is_empty()` could hold for reasons that have nothing
    /// to do with the check: measured by deleting `self.confirm_unchanged(&target)?;` from
    /// `run`, the swap test's error became `language server zuno-lsp-handoff-probe is
    /// unavailable`, i.e. the swapped path really did reach the handoff.
    #[tokio::test]
    async fn the_same_call_reaches_the_handoff_when_nothing_is_swapped() {
        let base = tempfile::tempdir().expect("base");
        let workspace = base.path().join("ws");
        std::fs::create_dir_all(workspace.join("src")).expect("create the workspace");
        std::fs::write(workspace.join("src/main.rs"), "fn main() {}\n").expect("write main.rs");

        let (manager, tool) = tool_with_a_matching_server(&workspace, &workspace);
        let recorder = Arc::new(Recorder::default());
        let error = tool
            .run(
                params("src/main.rs"),
                context(Arc::clone(&recorder) as Arc<dyn PermissionAsker>),
            )
            .await
            .expect_err("the probe server cannot start, so the call still fails");

        assert_eq!(recorder.keys(), vec!["lsp".to_owned()]);
        let ToolError::Failed { source, .. } = &error else {
            panic!("expected a failure from the server, got {error:?}");
        };
        let detail = source.to_string();
        assert!(
            !detail.contains("not to the authorized"),
            "nothing was swapped, so the handoff check must not fire: {detail}"
        );
        assert!(
            !manager.status().await.is_empty(),
            "`touch_file` must be reachable on this fixture, or the swap test's \
             \"never reached\" assertion proves nothing"
        );
    }

    #[tokio::test]
    async fn an_invalid_position_is_refused_before_any_permission_is_asked() {
        let workspace = tempfile::tempdir().expect("workspace");
        let recorder = Arc::new(Recorder::default());
        let tool = tool(workspace.path(), workspace.path());
        let error = tool
            .run(
                LspParams {
                    line: 0,
                    ..params("src/main.rs")
                },
                context(Arc::clone(&recorder) as Arc<dyn PermissionAsker>),
            )
            .await
            .expect_err("a zero line is not a one-based position");

        assert!(matches!(error, ToolError::InvalidArgs { .. }));
        assert!(recorder.keys().is_empty());
    }

    #[tokio::test]
    async fn workspace_symbol_still_escalates_because_it_opens_the_file_it_is_given() {
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let file = outside.path().join("notes.rs");
        std::fs::write(&file, "fn f() {}\n").expect("write file");

        let recorder = Arc::new(Recorder::default());
        let tool = tool(workspace.path(), workspace.path());
        let _ = tool
            .run(
                LspParams {
                    operation: LspOperation::WorkspaceSymbol,
                    query: Some("f".to_owned()),
                    ..params(&file.to_string_lossy())
                },
                context(Arc::clone(&recorder) as Arc<dyn PermissionAsker>),
            )
            .await;

        assert_eq!(
            recorder.keys(),
            vec!["external_directory".to_owned(), "lsp".to_owned()],
            "`workspaceSymbol` reaches `touch_file` like every other operation"
        );
    }

    /// Inverts what the previous fix pinned as a contract.
    ///
    /// A decision about one string and a handoff of another is a hint, not a boundary.
    /// The only thing allowed to differ between them is the root prefix, respelled for
    /// the manager's own `starts_with` root walk, and even that is re-resolved and
    /// compared before the handoff.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_path_handed_to_the_manager_resolves_to_the_path_that_was_authorized() {
        use std::path::Component;

        let base = tempfile::tempdir().expect("base");
        let workspace = base.path().join("ws");
        let outside = base.path().join("outside");
        std::fs::create_dir_all(workspace.join("src")).expect("create workspace");
        std::fs::create_dir_all(outside.join("dir")).expect("create outside");
        std::fs::write(outside.join("file.rs"), "fn f() {}\n").expect("write the outside file");
        std::fs::write(workspace.join("src/main.rs"), "fn main() {}\n").expect("write main.rs");
        std::os::unix::fs::symlink(outside.join("dir"), workspace.join("lnk")).expect("symlink");

        let tool = tool(&workspace, &workspace);
        let escaped = tool
            .resolve_target(&workspace.join("lnk/../file.rs").to_string_lossy())
            .expect("a resolvable path");
        assert_eq!(escaped.absolute, resolved(&outside).join("file.rs"));
        assert_eq!(
            escaped.open_path, escaped.absolute,
            "an external target is handed exactly what the escalation named"
        );
        assert!(
            !escaped
                .open_path
                .components()
                .any(|component| component == Component::ParentDir),
            "an unresolved `..` must not reach the language server"
        );
        tool.confirm_unchanged(&escaped)
            .expect("an unchanged path is confirmed");

        // Inside a root, only the root prefix is respelled — and it still resolves to
        // the authorized file, which is what `confirm_unchanged` verifies.
        let inside = tool
            .resolve_target("src/../src/main.rs")
            .expect("a resolvable path");
        assert_eq!(inside.resource, "src/main.rs");
        assert_eq!(inside.open_path, workspace.join("src/main.rs"));
        assert_eq!(resolved(&inside.open_path), inside.absolute);
        assert!(inside.external_parent.is_none());
        tool.confirm_unchanged(&inside)
            .expect("an unchanged path is confirmed");
    }

    /// A path that stops resolving to the authorized file fails the call.
    ///
    /// The window is real: the two asks can wait on a human while a concurrent `shell`
    /// job replaces a component of the path.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_path_swapped_after_authorization_is_refused_at_the_handoff() {
        let base = tempfile::tempdir().expect("base");
        let workspace = base.path().join("ws");
        let outside = base.path().join("outside");
        std::fs::create_dir_all(workspace.join("src")).expect("create workspace");
        std::fs::create_dir_all(&outside).expect("create outside");
        std::fs::write(workspace.join("src/main.rs"), "fn main() {}\n").expect("write main.rs");
        std::fs::write(outside.join("main.rs"), "const SECRET: u8 = 1;\n").expect("write secret");

        let tool = tool(&workspace, &workspace);
        let target = tool
            .resolve_target("src/main.rs")
            .expect("a resolvable path");
        tool.confirm_unchanged(&target)
            .expect("nothing has moved yet");

        // What another task could do while the prompt is on screen.
        std::fs::remove_dir_all(workspace.join("src")).expect("remove the real directory");
        std::os::unix::fs::symlink(&outside, workspace.join("src")).expect("plant the link");

        let error = tool
            .confirm_unchanged(&target)
            .expect_err("the authorized file moved");
        assert!(
            matches!(error, ToolError::Failed { .. }),
            "expected the handoff to be refused, got {error:?}"
        );
    }

    /// Asserts the invariant rather than either platform's answer: whatever file
    /// [`Path::canonicalize`] says `argument` names, that is the file the escalation and
    /// the `lsp` resource are about.
    ///
    /// POSIX applies `..` to what a link resolved to and the Win32 path layer folds `..`
    /// before the object manager sees a non-verbatim path, so the two platforms are
    /// *designed* to answer differently for the same spelling. A test that hard-codes one
    /// answer is untestable on the other host and is a guess wherever it has never run.
    /// This body derives the expectation from the platform itself, so it is correct under
    /// either rule and the same assertion runs everywhere.
    async fn the_asks_follow_the_platforms_own_resolution(workspace: &Path, argument: &Path) {
        let expected = argument
            .canonicalize()
            .expect("the fixture argument resolves");
        let root = workspace.canonicalize().expect("the workspace resolves");
        let recorder = Arc::new(Recorder::default());
        let tool = tool(workspace, workspace);
        let _ = tool
            .run(
                params(&argument.to_string_lossy()),
                context(Arc::clone(&recorder) as Arc<dyn PermissionAsker>),
            )
            .await;

        let inside = expected.starts_with(&root);
        let expected_keys = if inside {
            vec!["lsp".to_owned()]
        } else {
            vec!["external_directory".to_owned(), "lsp".to_owned()]
        };
        assert_eq!(
            recorder.keys(),
            expected_keys,
            "{} resolves to {}, which is {} the workspace",
            wire_path(argument),
            wire_path(&expected),
            if inside { "inside" } else { "outside" }
        );
        let asks = recorder.asks();
        if !inside {
            let parent = expected.parent().expect("a resolved file has a parent");
            assert_eq!(asks[0].patterns, vec![wire_path(&parent.join("*"))]);
        }
        let resource = if inside {
            wire_path(
                expected
                    .strip_prefix(&root)
                    .expect("an inside target strips"),
            )
        } else {
            wire_path(&expected)
        };
        assert_eq!(
            asks[usize::from(!inside)].patterns,
            vec![resource],
            "the resource has to name the file the platform resolved to"
        );
    }

    /// The invariant on this host, where `..` after a symlink leaves the workspace.
    ///
    /// Exercises [`the_asks_follow_the_platforms_own_resolution`] itself, so the Windows
    /// arm below is not the only caller of a helper that has never run.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_asks_follow_the_platforms_own_resolution_through_a_unix_symlink() {
        let base = tempfile::tempdir().expect("base");
        let workspace = base.path().join("ws");
        let outside = base.path().join("outside");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        std::fs::create_dir_all(outside.join("dir")).expect("create outside");
        std::fs::write(outside.join("file.rs"), "const SECRET: u8 = 1;\n").expect("write outside");
        std::fs::write(workspace.join("file.rs"), "fn f() {}\n").expect("write workspace file");
        std::os::unix::fs::symlink(outside.join("dir"), workspace.join("lnk")).expect("plant link");

        the_asks_follow_the_platforms_own_resolution(&workspace, &workspace.join("lnk/../file.rs"))
            .await;
    }

    /// The same shape across a Windows directory junction.
    ///
    /// The expected outcome is *not* written down here, because the point at issue is
    /// exactly what Win32 does with `..` after a junction and nothing that has ever run
    /// can settle it. The previous version of this test asserted no escalation and the
    /// pattern `["file.rs"]` — a guess that would have been a red CI run rather than a
    /// hole if Win32 resolved through the junction instead. Production never encodes the
    /// answer either: [`resolve_kernel_path`] asks [`Path::canonicalize`], so the boundary
    /// decision follows the platform whichever way it goes. Creating the link needs
    /// Developer Mode or `SeCreateSymbolicLinkPrivilege`; where that is unavailable the
    /// test says what it could not observe.
    #[cfg(windows)]
    #[tokio::test]
    async fn the_asks_follow_the_platforms_own_resolution_through_a_windows_junction() {
        let base = tempfile::tempdir().expect("base");
        let workspace = base.path().join("ws");
        let outside = base.path().join("outside");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        std::fs::create_dir_all(outside.join("dir")).expect("create outside");
        std::fs::write(outside.join("file.rs"), "const SECRET: u8 = 1;\n").expect("write outside");
        std::fs::write(workspace.join("file.rs"), "fn f() {}\n").expect("write workspace file");
        if std::os::windows::fs::symlink_dir(outside.join("dir"), workspace.join("lnk")).is_err() {
            eprintln!(
                "SKIPPED the_asks_follow_the_platforms_own_resolution_through_a_windows_junction: \
                 a directory symlink needs Developer Mode or SeCreateSymbolicLinkPrivilege"
            );
            return;
        }

        the_asks_follow_the_platforms_own_resolution(&workspace, &workspace.join("lnk/../file.rs"))
            .await;
    }

    #[tokio::test]
    async fn the_lsp_tool_still_authorizes_when_everything_is_allowed() {
        let workspace = tempfile::tempdir().expect("workspace");
        let tool = tool(workspace.path(), workspace.path());
        let error = tool
            .run(
                params("src/main.rs"),
                context(Arc::new(AllowAll) as Arc<dyn PermissionAsker>),
            )
            .await
            .expect_err("no such file exists");
        assert!(
            matches!(error, ToolError::Failed { .. }),
            "an allowed call must fail on the missing file, not on permission"
        );
    }

    /// A `python3` that can run the stub server, or `None` for an honest skip.
    ///
    /// The same three-state shape as `tests/live_rust_analyzer.rs`: a name that resolves
    /// but cannot execute is not the same as no name at all.
    fn stub_python() -> Option<PathBuf> {
        for candidate in ["python3", "python"] {
            let Ok(paths) = which::which_all(candidate) else {
                continue;
            };
            for path in paths {
                let usable = std::process::Command::new(&path)
                    .args(["-c", "import json, sys"])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|status| status.success());
                if usable {
                    return Some(path);
                }
            }
        }
        None
    }

    /// A minimal language server: initializes, ignores notifications, answers every
    /// request with `null`.
    fn write_stub_server(path: &Path) {
        std::fs::write(
            path,
            r#"import json, sys
def read():
    size = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b'\r\n', b'\n'):
            break
        if line.lower().startswith(b'content-length:'):
            size = int(line.split(b':', 1)[1].strip())
    return json.loads(sys.stdin.buffer.read(size))
def send(value):
    body = json.dumps(value, separators=(',', ':')).encode()
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n' % len(body)).encode() + body)
    sys.stdout.buffer.flush()
while True:
    message = read()
    if message is None:
        break
    if message.get('method') == 'initialize':
        send({'jsonrpc':'2.0','id':message['id'],'result':{'capabilities':{}}})
    elif 'id' in message:
        send({'jsonrpc':'2.0','id':message['id'],'result':None})
"#,
        )
        .expect("write the stub language server");
    }

    /// The whole call, end to end, against a language server that really answers.
    ///
    /// Nothing else in this crate drives `run` to a `ToolOutput`: every other tool test
    /// stops at `has_server` or at a server that cannot start, so the handoff itself —
    /// `open_path` -> `Manager::touch_file` -> `file_uri` -> `textDocument/hover` — and
    /// [`LspTool::title`] had no coverage at all. Both are asserted here against the
    /// verifier's two-root layout (`directory = <wt>/crates/inner`), so the string the
    /// user reads, the string permission rules are matched against, and the path the
    /// server was actually given are pinned together in one run.
    #[tokio::test]
    async fn a_full_call_reaches_a_real_server_and_renders_the_anchored_spelling() {
        let Some(python) = stub_python() else {
            eprintln!(
                "SKIPPED a_full_call_reaches_a_real_server_and_renders_the_anchored_spelling: \
                 no usable python3 on PATH"
            );
            return;
        };
        let worktree = tempfile::tempdir().expect("worktree");
        let directory = worktree.path().join("crates/inner");
        std::fs::create_dir_all(directory.join("src")).expect("create the inner crate");
        let script = worktree.path().join("server.py");
        write_stub_server(&script);
        let source = directory.join("src/probe.mine");
        std::fs::write(&source, "probe\n").expect("write the source file");

        let config: zuno_config::schema::lsp::LspConfig = serde_json::from_value(json!({
            "zuno-lsp-stub": {
                "command": [python.to_string_lossy(), script.to_string_lossy(), "--stdio"],
                "extensions": [".mine"]
            }
        }))
        .expect("a custom server entry");
        let registry = Arc::new(crate::ServerRegistry::offline(
            &zuno_catalog::lsp_config::ResolvedLsp::resolve(Some(&config)),
        ));
        let manager = Arc::new(Manager::new(
            &directory,
            registry,
            crate::RestartPolicy::default(),
            std::num::NonZeroUsize::new(4).expect("non-zero"),
        ));
        let tool = LspTool::new(Arc::clone(&manager), &directory, worktree.path());

        let recorder = Arc::new(Recorder::default());
        let output = tool
            .run(
                params("src/probe.mine"),
                context(Arc::clone(&recorder) as Arc<dyn PermissionAsker>),
            )
            .await
            .expect("the stub server answers hover");
        manager.shutdown().await;

        assert_eq!(
            recorder.keys(),
            vec!["lsp".to_owned()],
            "an in-boundary file owes no escalation"
        );
        assert_eq!(
            recorder.asks()[0].patterns,
            vec!["crates/inner/src/probe.mine".to_owned()],
            "the resource is anchored at the worktree, not at whichever root matched"
        );
        assert_eq!(
            output.title, "hover crates/inner/src/probe.mine:1:1",
            "the rendered spelling is the spelling a rule has to name"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_inside_the_workspace_cannot_smuggle_a_file_from_outside_it() {
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("config"), "IdentityFile ~/.ssh/id\n")
            .expect("write secret");
        let link = workspace.path().join("linked");
        std::os::unix::fs::symlink(outside.path(), &link).expect("symlink");

        let recorder = Arc::new(Recorder::default());
        let tool = tool(workspace.path(), workspace.path());
        let _ = tool
            .run(
                params(&link.join("config").to_string_lossy()),
                context(Arc::clone(&recorder) as Arc<dyn PermissionAsker>),
            )
            .await;

        assert_eq!(
            recorder.keys(),
            vec!["external_directory".to_owned(), "lsp".to_owned()],
            "a lexical prefix test would have called this path a workspace file"
        );
        assert_eq!(
            recorder.asks()[0].patterns,
            vec![wire_path(&resolved(outside.path()).join("*"))]
        );
    }

    /// The escape the verifier reproduced: `..` placed immediately after a directory
    /// symlink that leaves the workspace.
    ///
    /// POSIX applies `..` to the directory the link resolved to, so the kernel reads
    /// `<outside>/file.rs`, while folding `lnk/..` lexically first names `<ws>` and the
    /// containment test then succeeds. Three characters added to the input the previous
    /// fix claimed to cover.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_parent_traversal_after_a_workspace_symlink_escalates_on_what_the_kernel_reaches() {
        let base = tempfile::tempdir().expect("base");
        let workspace = base.path().join("ws");
        let outside = base.path().join("outside");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        std::fs::create_dir_all(outside.join("dir")).expect("create outside");
        std::fs::write(
            outside.join("file.rs"),
            "const SECRET: &str = \"outside\";\n",
        )
        .expect("write the outside file");
        std::os::unix::fs::symlink(outside.join("dir"), workspace.join("lnk")).expect("symlink");

        let requested = workspace.join("lnk/../file.rs");
        // The premise, asserted rather than assumed: this spelling reads the outside
        // file, and the file a lexical fold of `lnk/..` names does not exist.
        assert_eq!(
            std::fs::read_to_string(&requested).expect("the kernel reads the outside file"),
            "const SECRET: &str = \"outside\";\n"
        );
        assert!(!workspace.join("file.rs").exists());

        let recorder = Arc::new(Recorder::default());
        let tool = tool(&workspace, &workspace);
        let _ = tool
            .run(
                params(&requested.to_string_lossy()),
                context(Arc::clone(&recorder) as Arc<dyn PermissionAsker>),
            )
            .await;

        assert_eq!(
            recorder.keys(),
            vec!["external_directory".to_owned(), "lsp".to_owned()],
            "`..` after a link out of the workspace must escalate on where the kernel lands"
        );
        let outside = outside.canonicalize().expect("canonical outside");
        let landed = wire_path(&outside.join("file.rs"));
        let asks = recorder.asks();
        assert_eq!(asks[0].patterns, vec![wire_path(&outside.join("*"))]);
        assert_eq!(asks[0].metadata["filepath"], Value::String(landed.clone()));
        assert_eq!(asks[1].patterns, vec![landed.clone()]);
        assert_eq!(asks[1].metadata["filepath"], Value::String(landed));
    }

    /// The second manifestation, which needs no planted link: the workspace root is
    /// itself a symlink, so `<ws>/../x.rs` lands beside the *real* root.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_workspace_root_grants_the_directory_the_kernel_reaches() {
        let base = tempfile::tempdir().expect("base");
        let home = base.path().join("home/u");
        let data = base.path().join("data");
        std::fs::create_dir_all(&home).expect("create home");
        std::fs::create_dir_all(data.join("proj")).expect("create the real project");
        std::fs::write(data.join("x.rs"), "fn x() {}\n").expect("write beside the real root");
        std::os::unix::fs::symlink(data.join("proj"), home.join("proj")).expect("symlink the root");

        let workspace = home.join("proj");
        let requested = workspace.join("../x.rs");
        assert_eq!(
            std::fs::read_to_string(&requested).expect("the kernel reads the real sibling"),
            "fn x() {}\n",
            "the lexical parent of the link is not the parent the kernel uses"
        );

        let recorder = Arc::new(Recorder::default());
        let tool = tool(&workspace, &workspace);
        let _ = tool
            .run(
                params(&requested.to_string_lossy()),
                context(Arc::clone(&recorder) as Arc<dyn PermissionAsker>),
            )
            .await;

        assert_eq!(
            recorder.keys(),
            vec!["external_directory".to_owned(), "lsp".to_owned()]
        );
        let data = data.canonicalize().expect("canonical data");
        assert_eq!(
            recorder.asks()[0].patterns,
            vec![wire_path(&data.join("*"))],
            "the directory granted has to be the directory read"
        );
    }

    /// A `..` the platform will not resolve fails closed instead of being reduced to a
    /// lexical guess that reads as contained.
    ///
    /// POSIX evaluates each component, so `<ws>/missing/..` has no answer while
    /// `missing` does not exist and the open would fail. Windows folds `..` in the
    /// Win32 path layer, where the same spelling is a legal name for `<ws>/file.rs`,
    /// which is why this expectation is POSIX-only.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_parent_traversal_through_a_missing_directory_is_refused_before_any_ask() {
        let workspace = tempfile::tempdir().expect("workspace");
        let recorder = Arc::new(Recorder::default());
        let tool = tool(workspace.path(), workspace.path());
        let error = tool
            .run(
                params(
                    &workspace
                        .path()
                        .join("missing/../file.rs")
                        .to_string_lossy(),
                ),
                context(Arc::clone(&recorder) as Arc<dyn PermissionAsker>),
            )
            .await
            .expect_err("a path with no resolvable answer must not be guessed at");

        assert!(
            matches!(error, ToolError::InvalidArgs { .. }),
            "expected the argument to be refused, got {error:?}"
        );
        assert!(
            recorder.keys().is_empty(),
            "nothing may be asked for a path the boundary cannot decide about"
        );
    }

    /// The root itself is spelled `.`, which is a literal rather than a reduction of the
    /// empty relative path, so the fail-closed spelling check must not refuse it.
    #[tokio::test]
    async fn the_root_itself_is_still_named_dot_and_is_not_refused_as_ambiguous() {
        let workspace = tempfile::tempdir().expect("workspace");
        let recorder = Arc::new(Recorder::default());
        let tool = tool(workspace.path(), workspace.path());
        let _ = tool
            .run(
                params(&workspace.path().to_string_lossy()),
                context(Arc::clone(&recorder) as Arc<dyn PermissionAsker>),
            )
            .await;

        assert_eq!(recorder.keys(), vec!["lsp".to_owned()]);
        assert_eq!(recorder.asks()[0].patterns, vec![".".to_owned()]);
    }

    /// `wire_path` maps `\` to `/`. On Linux and macOS `\` is a legal character in a file
    /// name, so that reduction merges the two different files `a\b.rs` and `a/b.rs` into
    /// one resource string, and a rule allowing `a/b.rs` would then govern a file the
    /// user never named. The reduction cannot be moved to the deny side from here —
    /// `zuno_permission::wildcard::wildcard_match` merges `\` in the rule's pattern too,
    /// so `wildcard_match("a\\b.rs", "a/b.rs")` is `true` and an extra unreduced pattern
    /// would allow alongside the reduced one. The call is refused instead, before any
    /// permission is asked and before any path reaches a language server.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_file_name_wire_path_would_merge_is_refused_because_no_rule_can_name_it() {
        let workspace = tempfile::tempdir().expect("workspace");
        let nested = workspace.path().join("a");
        std::fs::create_dir_all(&nested).expect("create the directory a rule would name");
        std::fs::write(nested.join("b.rs"), "fn real() {}\n").expect("write a/b.rs");
        // One component whose name is the six characters `a\b.rs`.
        let smuggled = workspace.path().join("a\\b.rs");
        std::fs::write(&smuggled, "fn other() {}\n").expect("write the backslash file");

        let recorder = Arc::new(Recorder::default());
        let tool = tool(workspace.path(), workspace.path());
        let error = tool
            .run(
                params(&smuggled.to_string_lossy()),
                context(Arc::clone(&recorder) as Arc<dyn PermissionAsker>),
            )
            .await
            .expect_err("a file no permission rule can name is refused");

        assert!(
            matches!(error, ToolError::InvalidArgs { .. }),
            "the argument is what is wrong, not the language server: {error}"
        );
        assert!(
            recorder.keys().is_empty(),
            "nothing may be asked about a resource string that names two files: {:?}",
            recorder.keys()
        );
        // The unaffected sibling still works, so the refusal is about the reduction and
        // not about the directory.
        let recorder = Arc::new(Recorder::default());
        let _ = tool
            .run(
                params(&nested.join("b.rs").to_string_lossy()),
                context(Arc::clone(&recorder) as Arc<dyn PermissionAsker>),
            )
            .await;

        assert_eq!(recorder.keys(), vec!["lsp".to_owned()]);
        assert_eq!(recorder.asks()[0].patterns, vec!["a/b.rs".to_owned()]);
    }

    /// Pins the *call site* of [`LspTool::confirm_unchanged`], not the method.
    ///
    /// Every other test of it calls the method directly on a `ResolvedTarget`, so deleting
    /// `self.confirm_unchanged(&target)?;` from `run` left the whole suite green — the one
    /// line a later refactor could drop with a passing suite as cover, restoring the
    /// original check-versus-use defect. This drives the real `run` and performs the swap
    /// from inside the permission prompt.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_refuses_a_path_swapped_while_the_permission_prompt_was_open() {
        let base = tempfile::tempdir().expect("base");
        let workspace = base.path().join("ws");
        let outside = base.path().join("outside");
        std::fs::create_dir_all(workspace.join("src")).expect("create the workspace");
        std::fs::create_dir_all(&outside).expect("create outside");
        std::fs::write(workspace.join("src/main.rs"), "fn main() {}\n").expect("write main.rs");
        std::fs::write(outside.join("main.rs"), "const SECRET: u8 = 1;\n").expect("write secret");

        let (manager, tool) = tool_with_a_matching_server(&workspace, &workspace);
        // The two assertions the "never reached" claim rests on: this manager *would*
        // have opened the file, and it has opened nothing yet.
        assert!(
            manager.has_server(&workspace.join("src/main.rs")),
            "the probe server must claim the file, or reaching `touch_file` is impossible \
             for reasons that have nothing to do with the handoff check"
        );
        assert!(manager.status().await.is_empty());

        let asker = Arc::new(SwapOnAsk {
            workspace: workspace.clone(),
            outside: outside.clone(),
            asked: Mutex::new(Vec::new()),
        });
        let error = tool
            .run(
                params("src/main.rs"),
                context(Arc::clone(&asker) as Arc<dyn PermissionAsker>),
            )
            .await
            .expect_err("the authorized path was replaced before the handoff");

        assert_eq!(
            asker.keys(),
            vec!["lsp".to_owned()],
            "the swap has to happen inside the ask, which is the window being tested"
        );
        let ToolError::Failed { source, .. } = &error else {
            panic!("expected the handoff to be refused, got {error:?}");
        };
        let detail = source.to_string();
        assert!(
            detail.contains("not to the authorized"),
            "expected `run` to re-resolve and refuse the swap; got {detail}"
        );
        assert!(
            manager.status().await.is_empty(),
            "`touch_file` must not have been reached: {:?}",
            manager.status().await
        );
    }

    /// A `filePath` whose resolve walk would cost a second of blocking CPU is refused
    /// before the walk starts.
    ///
    /// The verifier's input: 100,000 components, a 200 KB argument. Measured with the
    /// extracted [`resolve_kernel_path`], the walk is quadratic — 1.59 ms for 1,000
    /// components, 59.4 ms for 20,000, 947 ms for 100,000 — and `run` paid it twice, on a
    /// `ParallelSafe` tool, with no prompt in the way. Before the bound this argument
    /// resolved *successfully* (only the tail is missing), so the `lsp` ask was reached and
    /// the resource string was 200 KB wide.
    #[tokio::test]
    async fn an_oversized_file_path_is_refused_before_the_resolve_walk_runs() {
        let workspace = tempfile::tempdir().expect("workspace");
        let recorder = Arc::new(Recorder::default());
        let tool = tool(workspace.path(), workspace.path());
        let deep = format!("{}x.rs", "a/".repeat(100_000));

        let start = std::time::Instant::now();
        let error = tool
            .run(
                params(&deep),
                context(Arc::clone(&recorder) as Arc<dyn PermissionAsker>),
            )
            .await
            .expect_err("an argument this long names no file on any supported platform");
        let elapsed = start.elapsed();

        assert!(
            matches!(error, ToolError::InvalidArgs { .. }),
            "expected the argument to be refused, got {error:?}"
        );
        assert!(
            recorder.keys().is_empty(),
            "nothing may be asked about an argument that was never resolved: {:?}",
            recorder.keys()
        );
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "the refusal must not pay for the walk it is refusing: took {elapsed:?}"
        );
        // The refusal must not echo the argument back either, or saying no is itself a way
        // to put 200 KB into the transcript.
        let rendered = match &error {
            ToolError::InvalidArgs { source, .. } => source.to_string(),
            other => panic!("expected InvalidArgs, got {other:?}"),
        };
        assert!(
            rendered.len() < 1_024,
            "the refusal quoted {} bytes of a 200 KB argument",
            rendered.len()
        );
    }

    /// The Windows arm of [`spells_one_file`], executed on this host.
    ///
    /// [`Path::canonicalize`] returns the verbatim `\\?\C:\...` form on Windows and
    /// [`wire_path`] strips that prefix, so the arm has to accept it — refusing it would
    /// make `lsp` reject every ordinary Windows path, and nothing that had ever run
    /// checked that. It is a parameter rather than a `cfg!` exactly so the claim is
    /// checked where the tests run.
    #[test]
    fn the_windows_name_rules_accept_the_verbatim_form_canonicalize_returns() {
        assert!(
            spells_one_file_under(
                "C:/ws/src/main.rs",
                Path::new(r"\\?\C:\ws\src\main.rs"),
                NameRules::Windows
            ),
            "refusing the canonical Windows spelling would refuse every Windows call"
        );
        assert!(spells_one_file_under(
            "//server/share/src/main.rs",
            Path::new(r"\\?\UNC\server\share\src\main.rs"),
            NameRules::Windows
        ));
        // The same six bytes under the two platforms' rules. On Windows `a\b.rs` *is* the
        // two components `a` and `b.rs`, so the wire spelling names exactly one file; on
        // POSIX it is one component whose name contains a backslash, and the wire spelling
        // names the different file `a/b.rs` too.
        let backslash = Path::new("a\\b.rs");
        assert!(spells_one_file_under(
            "a/b.rs",
            backslash,
            NameRules::Windows
        ));
        assert!(!spells_one_file_under(
            "a/b.rs",
            backslash,
            NameRules::Posix
        ));
        #[cfg(unix)]
        assert_eq!(NameRules::current(), NameRules::Posix);
        #[cfg(windows)]
        assert_eq!(NameRules::current(), NameRules::Windows);
    }

    /// A root that bounds nothing stops counting as a root.
    ///
    /// `zuno_paths::resolve_project` reports the filesystem root as the project directory
    /// for a session outside any repository. Wired in as a worktree, every absolute path on
    /// the machine strips against it, so `lsp` raised no `external_directory` escalation
    /// for anything, anywhere, and spelled `/etc/shadow` as the rootless `etc/shadow`.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_filesystem_root_does_not_count_as_a_containment_boundary() {
        let base = tempfile::tempdir().expect("base");
        let workspace = base.path().join("ws");
        let outside = base.path().join("outside");
        std::fs::create_dir_all(&workspace).expect("create the workspace");
        std::fs::create_dir_all(&outside).expect("create outside");
        let secret = outside.join("secret.rs");
        std::fs::write(&secret, "const KEY: &str = \"k\";\n").expect("write the outside file");

        let recorder = Arc::new(Recorder::default());
        let tool = tool(&workspace, Path::new("/"));
        let _ = tool
            .run(
                params(&secret.to_string_lossy()),
                context(Arc::clone(&recorder) as Arc<dyn PermissionAsker>),
            )
            .await;

        assert_eq!(
            recorder.keys(),
            vec!["external_directory".to_owned(), "lsp".to_owned()],
            "`/` as a worktree must not make the whole filesystem in-boundary"
        );
        let asks = recorder.asks();
        assert_eq!(
            asks[0].patterns,
            vec![wire_path(&resolved(&outside).join("*"))]
        );
        assert_eq!(
            asks[1].patterns,
            vec![wire_path(&resolved(&secret))],
            "and the resource stays the full path rather than a rootless `tmp/...`"
        );
    }
}

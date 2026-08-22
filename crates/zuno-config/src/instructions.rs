//! Instruction-file discovery and the `instructions[]` loader.
//!
//! Port of `packages/opencode/src/session/instruction.ts` (opencode 1.18.13).
//! Instruction files are the `AGENTS.md`-style rules injected into *every*
//! prompt, so this module has two failure modes that both cost the user money on
//! every single turn: loading a file the TypeScript binary would not load
//! changes how the agent behaves, and loading the same file twice pays for the
//! tokens twice, forever.
//!
//! # Three mechanisms, deliberately kept apart
//!
//! **1. The global file** (`:60-63`, applied at `:115-120`) — `$CONFIG/AGENTS.md`,
//! else `~/.claude/CLAUDE.md`. The loop `break`s, so **at most one** global file
//! is ever loaded, and the Claude fallback disappears when Claude-Code
//! compatibility is off ([`InstructionOptions::claude_prompt_disabled`]).
//!
//! **2. The project filename cascade** (`:64-68`, applied at `:122-133`) — the
//! rule that is easiest to get subtly wrong. The oracle iterates *filenames*
//! and, for each, calls `findUp(name, directory, worktree)`, which returns
//! **every** ancestor level holding that name; the `break` then stops it from
//! trying the next filename:
//!
//! ```text
//! for (const file of instructionFiles) {                     // instruction.ts:123
//!   const matches = yield* fs.findUp(file, ctx.directory, ctx.worktree)
//!   if (matches.length > 0) { matches.forEach(add); break }   // :127-130
//! }
//! ```
//!
//! *first class wins* therefore means the first **filename** that exists
//! anywhere on the chain claims the whole chain. A repo with `sub/AGENTS.md` and
//! `root/AGENTS.md` loads **both**; a repo with `sub/CLAUDE.md` and
//! `root/AGENTS.md` loads **only** `root/AGENTS.md`. It is a cascade across
//! *filenames*, not a single-file pick — the oracle's own comment at `:122`
//! ("so we don't stack AGENTS.md/CLAUDE.md from every ancestor") is about mixing
//! the two names, not about collapsing the levels.
//!
//! **3. The upward append** (`:179-220`, [`Instructions::nearby`]) — a separate
//! mechanism, triggered when a *file* is read mid-session. It walks up from that
//! file's directory and attaches any instruction file it passes that is not
//! already accounted for. It uses [`Instructions::find`] (the first cascade
//! filename present in **one** directory, no walk) and applies three
//! independent guards — the system set, the paths already read this session, and
//! this message's claims — so each file is charged for exactly once.
//!
//! # `CONTEXT.md` is not here
//!
//! The oracle's cascade has a third filename, `CONTEXT.md`, marked
//! `// deprecated` at `:67`. This project rejects deprecated forms (todo 10), so
//! the cascade here is `AGENTS.md` → `CLAUDE.md` only. A deliberate, recorded
//! divergence.
//!
//! # Bounds
//!
//! Local reads run at [`LOCAL_CONCURRENCY`], remote fetches at
//! [`REMOTE_CONCURRENCY`], and every remote fetch is bounded by
//! [`REMOTE_TIMEOUT`] (`:96-99`). A remote instruction that hangs, 404s, or has
//! no DNS produces an [`InstructionWarning`] and is dropped from the result — it
//! never fails the load, because a flaky URL in a config file must not make the
//! agent unusable.

pub(crate) mod glob;

use crate::Config;
use futures::stream::StreamExt;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use zuno_paths::{Env, Layout, node_path, walk};

/// How many local instruction files are read at once (`instruction.ts:157`).
pub const LOCAL_CONCURRENCY: usize = 8;

/// How many remote instruction URLs are fetched at once (`instruction.ts:158`).
pub const REMOTE_CONCURRENCY: usize = 4;

/// The per-URL budget for a remote instruction (`instruction.ts:97`).
pub const REMOTE_TIMEOUT: Duration = Duration::from_secs(5);

/// The project-level filename cascade, in probe order.
///
/// The oracle's `instructionFiles` (`instruction.ts:64-68`) also lists
/// `CONTEXT.md`; see the module documentation for why this port stops here.
pub const INSTRUCTION_FILENAMES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

/// The filename probed inside the global config directory.
pub const GLOBAL_INSTRUCTION_FILENAME: &str = "AGENTS.md";

/// The Claude-Code global instruction file, relative to `$HOME`.
pub const CLAUDE_GLOBAL_RELATIVE: [&str; 2] = [".claude", "CLAUDE.md"];

/// `ZUNO_DISABLE_CLAUDE_CODE` — the broad switch
/// (`packages/opencode/src/effect/runtime-flags.ts:24`).
pub const ZUNO_DISABLE_CLAUDE_CODE: &str = "ZUNO_DISABLE_CLAUDE_CODE";

/// `ZUNO_DISABLE_CLAUDE_CODE_PROMPT` — the targeted switch
/// (`runtime-flags.ts:25`). Either variable disables the Claude instruction
/// files.
pub const ZUNO_DISABLE_CLAUDE_CODE_PROMPT: &str = "ZUNO_DISABLE_CLAUDE_CODE_PROMPT";

/// The header the oracle puts above every instruction body (`instruction.ts:162`).
const HEADER: &str = "Instructions from: ";

/// Which of the three mechanisms produced a path.
///
/// The oracle collapses all of these into one unordered `Set<string>`. Keeping
/// the provenance costs nothing and is what lets a caller explain to a user why a
/// file is in their prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Origin {
    /// `$CONFIG/AGENTS.md` or `~/.claude/CLAUDE.md`.
    Global,
    /// A hit from the project filename cascade.
    Project,
    /// A path produced by an `instructions[]` entry.
    Configured,
    /// A file attached by the upward walk from a file being read.
    Nearby,
}

/// A local instruction file, with the path the oracle would print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionPath {
    path: PathBuf,
    origin: Origin,
}

impl InstructionPath {
    /// The resolved absolute path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Which mechanism found it.
    #[must_use]
    pub fn origin(&self) -> Origin {
        self.origin
    }
}

/// An ordered, path-de-duplicated accumulator.
///
/// De-duplication is by **resolved identity**, not by the string a config file
/// happened to spell: `AGENTS.md`, `./AGENTS.md` and `/repo/AGENTS.md` are one
/// file, and so are two spellings that reach the same inode through a symlink.
/// The path *reported* is still the textually resolved one, so output matches the
/// oracle even where the canonical path differs (`/var` vs `/private/var`).
#[derive(Debug, Default)]
struct PathSet {
    ordered: Vec<InstructionPath>,
    seen: HashSet<PathBuf>,
}

impl PathSet {
    fn insert(&mut self, path: &Path, origin: Origin) -> bool {
        let resolved = resolve(path);
        if !self.seen.insert(identity(&resolved)) {
            return false;
        }
        self.ordered.push(InstructionPath {
            path: resolved,
            origin,
        });
        true
    }
}

/// `path.resolve(p)` — textual, filesystem-free and symlink-blind, exactly as
/// Node does it.
fn resolve(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if path.is_absolute() {
        return PathBuf::from(node_path::normalize(&text));
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    PathBuf::from(node_path::resolve(&cwd.to_string_lossy(), &[&text]))
}

/// The de-duplication key: the canonical path when the file exists, else the
/// resolved path. Two spellings of one file must never be charged twice.
fn identity(resolved: &Path) -> PathBuf {
    std::fs::canonicalize(resolved).unwrap_or_else(|_| resolved.to_path_buf())
}

/// Everything instruction discovery needs, with no hidden process state.
///
/// Production callers use [`InstructionOptions::from_process`]. Tests use
/// [`InstructionOptions::new`] with an explicit [`Env`], because mutating the
/// process environment is `unsafe` and this workspace forbids it.
#[derive(Debug, Clone)]
pub struct InstructionOptions {
    directory: PathBuf,
    worktree: Option<PathBuf>,
    layout: Layout,
    instructions: Vec<String>,
    claude_prompt_disabled: bool,
}

impl InstructionOptions {
    /// Build from an explicit environment snapshot and an already-merged
    /// `instructions` list.
    ///
    /// The list must be the one [`crate::discovery`] produced: that module owns
    /// the cross-layer concat-and-de-duplicate rule, and this module only
    /// consumes its output.
    #[must_use]
    pub fn new(
        directory: impl Into<PathBuf>,
        worktree: Option<impl Into<PathBuf>>,
        env: &Env,
        instructions: Vec<String>,
    ) -> Self {
        Self {
            directory: directory.into(),
            worktree: worktree.map(Into::into),
            layout: Layout::resolve(env),
            instructions,
            claude_prompt_disabled: env.flag(ZUNO_DISABLE_CLAUDE_CODE)
                || env.flag(ZUNO_DISABLE_CLAUDE_CODE_PROMPT),
        }
    }

    /// Build from an explicit environment and a merged [`Config`].
    #[must_use]
    pub fn from_config(
        directory: impl Into<PathBuf>,
        worktree: Option<impl Into<PathBuf>>,
        env: &Env,
        config: &Config,
    ) -> Self {
        Self::new(
            directory,
            worktree,
            env,
            config.instructions.clone().unwrap_or_default(),
        )
    }

    /// Build from the process environment and a merged [`Config`].
    #[must_use]
    pub fn from_process(
        directory: impl Into<PathBuf>,
        worktree: Option<impl Into<PathBuf>>,
        config: &Config,
    ) -> Self {
        Self::from_config(directory, worktree, &Env::from_process(), config)
    }

    /// Override the resolved layout, for a test that needs a fabricated config or
    /// home directory without touching the environment.
    #[must_use]
    pub fn with_layout(mut self, layout: Layout) -> Self {
        self.layout = layout;
        self
    }

    /// The directory the session is anchored at.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// The worktree the ancestor walks stop at, when there is one.
    #[must_use]
    pub fn worktree(&self) -> Option<&Path> {
        self.worktree.as_deref()
    }

    /// Whether `~/.claude/CLAUDE.md` and the `CLAUDE.md` cascade entry are
    /// suppressed — the oracle's `flags.disableClaudeCodePrompt`.
    #[must_use]
    pub fn claude_prompt_disabled(&self) -> bool {
        self.claude_prompt_disabled
    }

    /// Whether `ZUNO_DISABLE_PROJECT_CONFIG` suppressed project discovery.
    #[must_use]
    pub fn project_config_disabled(&self) -> bool {
        self.layout.project_config_disabled()
    }

    fn filenames(&self) -> Vec<&'static str> {
        INSTRUCTION_FILENAMES
            .iter()
            .copied()
            .filter(|name| !(self.claude_prompt_disabled && *name == "CLAUDE.md"))
            .collect()
    }

    fn global_files(&self) -> Vec<PathBuf> {
        let mut files = vec![
            self.layout
                .effective_config()
                .join(GLOBAL_INSTRUCTION_FILENAME),
        ];
        if !self.claude_prompt_disabled {
            let mut claude = self.layout.home().to_path_buf();
            claude.extend(CLAUDE_GLOBAL_RELATIVE);
            files.push(claude);
        }
        files
    }
}

/// The discovered instruction set: local paths in oracle order, plus the remote
/// URLs still to fetch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Instructions {
    paths: Vec<InstructionPath>,
    urls: Vec<String>,
}

impl Instructions {
    /// `Instruction.systemPaths` plus the URL partition of `Instruction.system`
    /// (`instruction.ts:110-155`).
    ///
    /// Runs the global probe, the project filename cascade, and `instructions[]`
    /// resolution, in that order, de-duplicating by resolved path throughout.
    /// Touches the filesystem but never the network.
    #[must_use]
    pub fn discover(options: &InstructionOptions) -> Self {
        let mut paths = PathSet::default();

        for file in options.global_files() {
            if file.exists() {
                paths.insert(&file, Origin::Global);
                break;
            }
        }

        if !options.project_config_disabled() {
            for name in options.filenames() {
                let matches = walk::up(&[name], &options.directory, options.worktree.as_deref());
                if matches.is_empty() {
                    continue;
                }
                for found in &matches {
                    paths.insert(found, Origin::Project);
                }
                break;
            }
        }

        let mut urls = Vec::new();
        let mut seen_urls = HashSet::new();
        for raw in &options.instructions {
            if is_remote(raw) {
                if seen_urls.insert(raw.clone()) {
                    urls.push(raw.clone());
                }
                continue;
            }
            for found in resolve_entry(options, raw) {
                paths.insert(&found, Origin::Configured);
            }
        }

        Self {
            paths: paths.ordered,
            urls,
        }
    }

    /// The local instruction files, in the order they will be rendered.
    #[must_use]
    pub fn paths(&self) -> &[InstructionPath] {
        &self.paths
    }

    /// The `http(s)://` entries, in config order.
    #[must_use]
    pub fn urls(&self) -> &[String] {
        &self.urls
    }

    /// Whether `path` is already part of the system set — the first of the three
    /// guards [`Instructions::nearby`] applies.
    #[must_use]
    pub fn contains(&self, path: &Path) -> bool {
        let key = identity(&resolve(path));
        self.paths.iter().any(|entry| identity(entry.path()) == key)
    }

    /// `Instruction.system` (`instruction.ts:150-168`).
    ///
    /// Reads every local path at [`LOCAL_CONCURRENCY`] and fetches every URL at
    /// [`REMOTE_CONCURRENCY`], each with a [`REMOTE_TIMEOUT`] budget. Never
    /// fails: an unreadable file or an unreachable URL becomes an
    /// [`InstructionWarning`] and is dropped, because one bad entry in a config
    /// file must not make the agent unusable.
    pub async fn load(&self) -> LoadedInstructions {
        let mut loaded = read_all(self.paths.clone()).await;
        loaded.absorb(fetch_all(&self.urls).await);
        loaded
    }

    /// `Instruction.find` (`instruction.ts:170-177`) — the first cascade filename
    /// present in **one** directory, with no ancestor walk.
    #[must_use]
    pub fn find(options: &InstructionOptions, dir: &Path) -> Option<PathBuf> {
        options
            .filenames()
            .into_iter()
            .map(|name| resolve(&dir.join(name)))
            .find(|candidate| candidate.exists())
    }

    /// `Instruction.resolve` (`instruction.ts:179-220`) — the upward append.
    ///
    /// Walks from `filepath`'s directory towards `options.directory`, returning
    /// the instruction files passed on the way that are not already accounted
    /// for. A path is skipped when it is `filepath` itself, when it is in this
    /// system set, when `already_read` has it (the session already paid for it),
    /// or when `claims` has it (this message already attached it). Claimed paths
    /// are recorded in `claims`, so calling this repeatedly for one message
    /// yields a file exactly once.
    ///
    /// The bound is the oracle's `current.startsWith(root) && current !== root`
    /// — a **string prefix**, not an ancestry test, which is why a sibling
    /// directory whose name extends the root (`/repo` and `/repo-vendor`) counts
    /// as inside it. Reproduced deliberately; diverging here would surface as a
    /// differential failure rather than a fix.
    #[must_use]
    pub fn nearby(
        &self,
        options: &InstructionOptions,
        filepath: &Path,
        already_read: &HashSet<PathBuf>,
        claims: &mut UpwardClaims,
    ) -> Vec<InstructionPath> {
        let root = resolve(&options.directory).to_string_lossy().into_owned();
        let target = resolve(filepath);
        let already: HashSet<PathBuf> = already_read.iter().map(|path| identity(path)).collect();

        let mut found = Vec::new();
        let mut current = node_path::dirname(&target.to_string_lossy());

        while current.starts_with(&root) && current != root {
            if let Some(candidate) = Self::find(options, Path::new(&current))
                && candidate != target
                && !self.contains(&candidate)
                && !already.contains(&identity(&candidate))
                && claims.claim(&candidate)
            {
                found.push(InstructionPath {
                    path: candidate,
                    origin: Origin::Nearby,
                });
            }
            let parent = node_path::dirname(&current);
            if parent == current {
                break;
            }
            current = parent;
        }
        found
    }

    /// Read the files [`Instructions::nearby`] returned, at
    /// [`LOCAL_CONCURRENCY`].
    pub async fn load_nearby(paths: Vec<InstructionPath>) -> LoadedInstructions {
        read_all(paths).await
    }
}

/// The instruction files one assistant message has already attached.
///
/// The oracle keeps this in `state.claims: Map<MessageID, Set<string>>`
/// (`instruction.ts:74`) and clears it per message; owning it explicitly makes
/// "exactly once" a property the caller can assert instead of a hidden
/// invariant.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UpwardClaims {
    claimed: HashSet<PathBuf>,
}

impl UpwardClaims {
    /// A fresh claim set for one assistant message.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `path`, returning `false` when it was already claimed.
    pub fn claim(&mut self, path: &Path) -> bool {
        self.claimed.insert(identity(&resolve(path)))
    }

    /// `Instruction.clear` (`instruction.ts:105-108`) — drop the record when the
    /// assistant message ends.
    pub fn clear(&mut self) {
        self.claimed.clear();
    }

    /// How many distinct files this message has attached.
    #[must_use]
    pub fn len(&self) -> usize {
        self.claimed.len()
    }

    /// Whether this message has attached nothing yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.claimed.is_empty()
    }
}

/// One instruction body, with the source line the oracle prints above it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionText {
    source: String,
    content: String,
    origin: Option<Origin>,
}

impl InstructionText {
    /// The path or URL, exactly as it appears in the rendered header.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The file or response body.
    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }

    /// The mechanism that produced it; `None` for a remote entry.
    #[must_use]
    pub fn origin(&self) -> Option<Origin> {
        self.origin
    }

    /// `` `Instructions from: ${item}\n${content}` `` (`instruction.ts:162-165`).
    #[must_use]
    pub fn render(&self) -> String {
        format!("{HEADER}{}\n{}", self.source, self.content)
    }
}

/// Why an instruction entry was dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WarningKind {
    /// A local file could not be read.
    Unreadable(std::io::ErrorKind),
    /// The fetch exceeded [`REMOTE_TIMEOUT`].
    RemoteTimeout,
    /// The server answered with a non-success status
    /// (`HttpClient.filterStatusOk`, `instruction.ts:59`).
    RemoteStatus(u16),
    /// The request never completed: DNS, TLS, connection refused.
    RemoteTransport(String),
}

/// A non-fatal problem with one instruction entry.
///
/// The oracle swallows all of these into an empty string
/// (`instruction.ts:91-92,98-99`), which makes a typo in `instructions[]`
/// invisible. Surfacing them is a deliberate improvement; they are still never
/// fatal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionWarning {
    source: String,
    kind: WarningKind,
}

impl InstructionWarning {
    /// The path or URL that failed.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// What went wrong.
    #[must_use]
    pub fn kind(&self) -> &WarningKind {
        &self.kind
    }
}

impl std::fmt::Display for InstructionWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            WarningKind::Unreadable(kind) => write!(
                f,
                "instruction file {} could not be read: {kind}",
                self.source
            ),
            WarningKind::RemoteTimeout => write!(
                f,
                "instruction {} did not respond within {}s and was skipped",
                self.source,
                REMOTE_TIMEOUT.as_secs()
            ),
            WarningKind::RemoteStatus(status) => write!(
                f,
                "instruction {} returned HTTP {status} and was skipped",
                self.source
            ),
            WarningKind::RemoteTransport(detail) => write!(
                f,
                "instruction {} was unreachable and was skipped: {detail}",
                self.source
            ),
        }
    }
}

/// The result of a load: the bodies that arrived, and why the rest did not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadedInstructions {
    entries: Vec<InstructionText>,
    warnings: Vec<InstructionWarning>,
}

impl LoadedInstructions {
    /// The bodies that arrived, in render order.
    #[must_use]
    pub fn entries(&self) -> &[InstructionText] {
        &self.entries
    }

    /// The entries that were dropped.
    #[must_use]
    pub fn warnings(&self) -> &[InstructionWarning] {
        &self.warnings
    }

    /// The `Instruction.system` return value: one rendered block per body.
    #[must_use]
    pub fn rendered(&self) -> Vec<String> {
        self.entries.iter().map(InstructionText::render).collect()
    }

    fn absorb(&mut self, other: Self) {
        self.entries.extend(other.entries);
        self.warnings.extend(other.warnings);
    }

    fn push_warning(&mut self, source: String, kind: WarningKind) {
        let warning = InstructionWarning { source, kind };
        tracing::warn!(instruction = %warning.source(), "{warning}");
        self.warnings.push(warning);
    }
}

fn is_remote(entry: &str) -> bool {
    entry.starts_with("https://") || entry.starts_with("http://")
}

/// `path.basename` for the shapes reachable here: `entry` is always absolute and
/// never has a trailing separator, because it came from a config string joined
/// against `$HOME` or written absolute by the user.
fn basename(entry: &str) -> &str {
    match entry.rsplit_once('/') {
        Some((_, name)) => name,
        None => entry,
    }
}

/// `instruction.ts:137-147` — a `~/` entry is expanded against `$HOME`, an
/// absolute entry is globbed inside its own directory, and a relative entry is
/// globbed up the ancestor chain.
fn resolve_entry(options: &InstructionOptions, raw: &str) -> Vec<PathBuf> {
    let entry = match raw.strip_prefix("~/") {
        Some(rest) => node_path::join(&options.layout.home().to_string_lossy(), rest),
        None => raw.to_owned(),
    };

    if Path::new(&entry).is_absolute() {
        let directory = node_path::dirname(&entry);
        return glob::files(basename(&entry), Path::new(&directory), false);
    }

    // `relative()` (`instruction.ts:78-88`): with project config disabled the
    // walk collapses to the global config directory, so a relative entry can
    // never reach the user's repository.
    if options.project_config_disabled() {
        let config = options.layout.effective_config();
        return glob::up(&entry, config, Some(config));
    }
    glob::up(&entry, &options.directory, options.worktree.as_deref())
}

async fn read_all(paths: Vec<InstructionPath>) -> LoadedInstructions {
    let results = futures::stream::iter(paths.into_iter().map(|entry| async move {
        let outcome = tokio::fs::read_to_string(entry.path()).await;
        (entry, outcome)
    }))
    .buffered(LOCAL_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    let mut loaded = LoadedInstructions::default();
    for (entry, outcome) in results {
        let source = entry.path().to_string_lossy().into_owned();
        match outcome {
            Ok(content) if content.is_empty() => {}
            Ok(content) => loaded.entries.push(InstructionText {
                source,
                content,
                origin: Some(entry.origin()),
            }),
            Err(error) => loaded.push_warning(source, WarningKind::Unreadable(error.kind())),
        }
    }
    loaded
}

async fn fetch_all(urls: &[String]) -> LoadedInstructions {
    let mut loaded = LoadedInstructions::default();
    if urls.is_empty() {
        return loaded;
    }

    let client = match zuno_network::client_builder()
        .timeout(REMOTE_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            for url in urls {
                loaded.push_warning(url.clone(), WarningKind::RemoteTransport(error.to_string()));
            }
            return loaded;
        }
    };

    let results = futures::stream::iter(urls.iter().cloned().map(|url| {
        let client = client.clone();
        async move {
            let outcome = fetch_one(&client, &url).await;
            (url, outcome)
        }
    }))
    .buffered(REMOTE_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    for (url, outcome) in results {
        match outcome {
            Ok(content) if content.is_empty() => {}
            Ok(content) => loaded.entries.push(InstructionText {
                source: url,
                content,
                origin: None,
            }),
            Err(kind) => loaded.push_warning(url, kind),
        }
    }
    loaded
}

/// One remote instruction, bounded end to end by [`REMOTE_TIMEOUT`].
///
/// The budget wraps the body read as well as the response headers: a server that
/// answers `200` and then stalls mid-body would otherwise hang the turn. That is
/// what `reqwest`'s own per-request timeout already guards, and the outer `tokio`
/// timeout makes unconditional.
async fn fetch_one(client: &reqwest::Client, url: &str) -> Result<String, WarningKind> {
    let request = async {
        let response = client.get(url).send().await.map_err(transport_or_timeout)?;
        let status = response.status();
        if !status.is_success() {
            return Err(WarningKind::RemoteStatus(status.as_u16()));
        }
        response.text().await.map_err(transport_or_timeout)
    };

    match tokio::time::timeout(REMOTE_TIMEOUT, request).await {
        Ok(result) => result,
        Err(_) => Err(WarningKind::RemoteTimeout),
    }
}

/// `reqwest`'s own per-request budget is the same 5s, so it usually trips before
/// the outer `tokio` timeout does. Without this the two paths would report the
/// same event under two different kinds, and a hanging server would look like a
/// DNS failure.
fn transport_or_timeout(error: reqwest::Error) -> WarningKind {
    if error.is_timeout() {
        return WarningKind::RemoteTimeout;
    }
    WarningKind::RemoteTransport(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use zuno_paths::env::{HOME, XDG_CONFIG_HOME};

    fn env_for(root: &Path) -> Env {
        Env::empty()
            .with(HOME, root.join("home").to_string_lossy().into_owned())
            .with(
                XDG_CONFIG_HOME,
                root.join("home/.config").to_string_lossy().into_owned(),
            )
    }

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, body).expect("write");
    }

    fn options_for(
        root: &Path,
        directory: PathBuf,
        instructions: Vec<String>,
    ) -> InstructionOptions {
        InstructionOptions::new(
            directory,
            Some(root.join("repo")),
            &env_for(root),
            instructions,
        )
    }

    #[test]
    fn the_cascade_excludes_context_md() {
        assert_eq!(INSTRUCTION_FILENAMES, ["AGENTS.md", "CLAUDE.md"]);
        assert!(!INSTRUCTION_FILENAMES.contains(&"CONTEXT.md"));
    }

    #[test]
    fn the_bounds_are_the_oracle_numbers() {
        assert_eq!(LOCAL_CONCURRENCY, 8);
        assert_eq!(REMOTE_CONCURRENCY, 4);
        assert_eq!(REMOTE_TIMEOUT, Duration::from_secs(5));
    }

    #[test]
    fn either_claude_flag_drops_the_claude_entries() {
        let root = tempfile::tempdir().expect("tempdir");
        for flag in [ZUNO_DISABLE_CLAUDE_CODE, ZUNO_DISABLE_CLAUDE_CODE_PROMPT] {
            let env = env_for(root.path()).with(flag, "true");
            let options = InstructionOptions::new(root.path(), None::<PathBuf>, &env, Vec::new());
            assert!(options.claude_prompt_disabled(), "{flag}");
            assert_eq!(options.filenames(), vec!["AGENTS.md"], "{flag}");
            assert_eq!(options.global_files().len(), 1, "{flag}");
        }

        let enabled = InstructionOptions::new(
            root.path(),
            None::<PathBuf>,
            &env_for(root.path()),
            Vec::new(),
        );
        assert!(!enabled.claude_prompt_disabled());
        assert_eq!(enabled.filenames(), vec!["AGENTS.md", "CLAUDE.md"]);
        assert_eq!(enabled.global_files().len(), 2);
    }

    /// Only one global file is ever loaded: the loop `break`s at the first hit
    /// (`instruction.ts:115-120`).
    #[test]
    fn the_global_probe_stops_at_the_first_hit() {
        let root = tempfile::tempdir().expect("tempdir");
        let config_agents = root.path().join("home/.config/zuno/AGENTS.md");
        write(&config_agents, "global agents");
        write(&root.path().join("home/.claude/CLAUDE.md"), "global claude");
        fs::create_dir_all(root.path().join("repo")).expect("mkdir repo");

        let found = Instructions::discover(&options_for(
            root.path(),
            root.path().join("repo"),
            Vec::new(),
        ));
        let globals: Vec<_> = found
            .paths()
            .iter()
            .filter(|entry| entry.origin() == Origin::Global)
            .collect();
        assert_eq!(globals.len(), 1);
        assert_eq!(globals[0].path(), resolve(&config_agents));
    }

    #[test]
    fn the_claude_global_is_the_fallback_when_agents_md_is_absent() {
        let root = tempfile::tempdir().expect("tempdir");
        let claude = root.path().join("home/.claude/CLAUDE.md");
        write(&claude, "global claude");
        fs::create_dir_all(root.path().join("repo")).expect("mkdir repo");

        let found = Instructions::discover(&options_for(
            root.path(),
            root.path().join("repo"),
            Vec::new(),
        ));
        assert_eq!(found.paths()[0].path(), resolve(&claude));
    }

    /// The heart of the task: the first *filename* claims the chain, and the
    /// second filename is never probed.
    #[test]
    fn the_first_filename_class_wins_and_claims_every_level() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = root.path().join("repo");
        write(&repo.join("AGENTS.md"), "root agents");
        write(&repo.join("CLAUDE.md"), "root claude");
        write(&repo.join("sub/AGENTS.md"), "sub agents");
        write(&repo.join("sub/CLAUDE.md"), "sub claude");

        let found = Instructions::discover(&options_for(root.path(), repo.join("sub"), Vec::new()));
        let project: Vec<&Path> = found
            .paths()
            .iter()
            .filter(|entry| entry.origin() == Origin::Project)
            .map(InstructionPath::path)
            .collect();
        assert_eq!(
            project,
            vec![
                resolve(&repo.join("sub/AGENTS.md")).as_path(),
                resolve(&repo.join("AGENTS.md")).as_path(),
            ]
        );
        assert!(
            !found.contains(&repo.join("CLAUDE.md")),
            "CLAUDE.md must not be loaded once AGENTS.md exists"
        );
    }

    /// The cascade falls through to `CLAUDE.md` only when no level has an
    /// `AGENTS.md` at all — a nearer `CLAUDE.md` does not beat a further
    /// `AGENTS.md`.
    #[test]
    fn a_nearer_claude_md_does_not_beat_a_further_agents_md() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = root.path().join("repo");
        write(&repo.join("AGENTS.md"), "root agents");
        write(&repo.join("sub/CLAUDE.md"), "sub claude");

        let found = Instructions::discover(&options_for(root.path(), repo.join("sub"), Vec::new()));
        let project: Vec<&Path> = found
            .paths()
            .iter()
            .filter(|entry| entry.origin() == Origin::Project)
            .map(InstructionPath::path)
            .collect();
        assert_eq!(project, vec![resolve(&repo.join("AGENTS.md")).as_path()]);
    }

    #[test]
    fn claude_md_is_used_when_no_level_has_agents_md() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = root.path().join("repo");
        write(&repo.join("CLAUDE.md"), "root claude");
        write(&repo.join("sub/CLAUDE.md"), "sub claude");

        let found = Instructions::discover(&options_for(root.path(), repo.join("sub"), Vec::new()));
        assert_eq!(found.paths().len(), 2);
    }

    #[test]
    fn disabling_project_config_skips_the_cascade_entirely() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = root.path().join("repo");
        write(&repo.join("AGENTS.md"), "root agents");

        let env = env_for(root.path()).with("ZUNO_DISABLE_PROJECT_CONFIG", "true");
        let options = InstructionOptions::new(&repo, Some(repo.clone()), &env, Vec::new());
        assert!(options.project_config_disabled());
        assert!(Instructions::discover(&options).paths().is_empty());
    }

    #[test]
    fn configured_entries_resolve_as_globs_tildes_and_urls() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = root.path().join("repo");
        write(&repo.join("docs/a.md"), "a");
        write(&repo.join("docs/b.md"), "b");
        write(&root.path().join("home/rules.md"), "home rules");

        let found = Instructions::discover(&options_for(
            root.path(),
            repo.clone(),
            vec![
                "docs/*.md".to_owned(),
                "~/rules.md".to_owned(),
                "https://example.invalid/x.md".to_owned(),
                "http://example.invalid/y.md".to_owned(),
            ],
        ));
        let configured: Vec<&Path> = found
            .paths()
            .iter()
            .filter(|entry| entry.origin() == Origin::Configured)
            .map(InstructionPath::path)
            .collect();
        assert_eq!(
            configured,
            vec![
                resolve(&repo.join("docs/a.md")).as_path(),
                resolve(&repo.join("docs/b.md")).as_path(),
                resolve(&root.path().join("home/rules.md")).as_path(),
            ]
        );
        assert_eq!(
            found.urls(),
            [
                "https://example.invalid/x.md".to_owned(),
                "http://example.invalid/y.md".to_owned()
            ]
        );
    }

    /// Three spellings of one file cost one file. A duplicate here is charged on
    /// every turn for the life of the session.
    #[test]
    fn one_file_spelled_three_ways_is_loaded_once() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = root.path().join("repo");
        write(&repo.join("AGENTS.md"), "root agents");
        let absolute = resolve(&repo.join("AGENTS.md"))
            .to_string_lossy()
            .into_owned();

        let found = Instructions::discover(&options_for(
            root.path(),
            repo.clone(),
            vec!["AGENTS.md".to_owned(), "./AGENTS.md".to_owned(), absolute],
        ));
        assert_eq!(found.paths().len(), 1);
    }

    #[test]
    fn a_duplicate_url_is_fetched_once() {
        let root = tempfile::tempdir().expect("tempdir");
        let found = Instructions::discover(&options_for(
            root.path(),
            root.path().to_path_buf(),
            vec![
                "https://example.invalid/x.md".to_owned(),
                "https://example.invalid/x.md".to_owned(),
            ],
        ));
        assert_eq!(found.urls().len(), 1);
    }

    #[test]
    fn find_probes_one_directory_in_cascade_order() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = root.path().join("repo");
        write(&repo.join("sub/CLAUDE.md"), "sub claude");
        write(&repo.join("sub/AGENTS.md"), "sub agents");
        write(&repo.join("other/CLAUDE.md"), "other claude");

        let options = options_for(root.path(), repo.clone(), Vec::new());
        assert_eq!(
            Instructions::find(&options, &repo.join("sub")),
            Some(resolve(&repo.join("sub/AGENTS.md")))
        );
        assert_eq!(
            Instructions::find(&options, &repo.join("other")),
            Some(resolve(&repo.join("other/CLAUDE.md")))
        );
        assert_eq!(Instructions::find(&options, &repo.join("nope")), None);
    }

    #[tokio::test]
    async fn an_empty_file_contributes_no_block() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = root.path().join("repo");
        write(&repo.join("AGENTS.md"), "");
        let loaded = Instructions::discover(&options_for(root.path(), repo.clone(), Vec::new()))
            .load()
            .await;
        assert!(loaded.entries().is_empty());
        assert!(loaded.warnings().is_empty());
    }

    #[tokio::test]
    async fn a_rendered_block_carries_the_oracle_header() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = root.path().join("repo");
        write(&repo.join("AGENTS.md"), "be careful");
        let loaded = Instructions::discover(&options_for(root.path(), repo.clone(), Vec::new()))
            .load()
            .await;
        let expected = format!(
            "Instructions from: {}\nbe careful",
            resolve(&repo.join("AGENTS.md")).display()
        );
        assert_eq!(loaded.rendered(), vec![expected]);
        assert_eq!(loaded.entries()[0].origin(), Some(Origin::Project));
    }

    #[test]
    fn the_upward_walk_attaches_a_parent_exactly_once() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = root.path().join("repo");
        write(&repo.join("pkg/AGENTS.md"), "pkg rules");
        write(&repo.join("pkg/src/main.rs"), "fn main() {}");

        let options = options_for(root.path(), repo.clone(), Vec::new());
        let system = Instructions::discover(&options);
        let mut claims = UpwardClaims::new();
        let already = HashSet::new();

        let first = system.nearby(
            &options,
            &repo.join("pkg/src/main.rs"),
            &already,
            &mut claims,
        );
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].path(), resolve(&repo.join("pkg/AGENTS.md")));
        assert_eq!(first[0].origin(), Origin::Nearby);
        assert_eq!(claims.len(), 1);

        let second = system.nearby(
            &options,
            &repo.join("pkg/src/lib.rs"),
            &already,
            &mut claims,
        );
        assert!(
            second.is_empty(),
            "a claimed file must not be attached twice in one message"
        );

        claims.clear();
        assert!(claims.is_empty());
        let after_clear = system.nearby(
            &options,
            &repo.join("pkg/src/lib.rs"),
            &already,
            &mut claims,
        );
        assert_eq!(after_clear.len(), 1, "a new message starts fresh");
    }

    /// A file already in the system set is never re-attached: the project cascade
    /// already paid for it.
    #[test]
    fn the_upward_walk_skips_system_paths() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = root.path().join("repo");
        write(&repo.join("pkg/AGENTS.md"), "pkg rules");
        write(&repo.join("pkg/src/main.rs"), "fn main() {}");

        let options = options_for(root.path(), repo.join("pkg"), Vec::new());
        let system = Instructions::discover(&options);
        assert!(system.contains(&repo.join("pkg/AGENTS.md")));

        let mut claims = UpwardClaims::new();
        assert!(
            system
                .nearby(
                    &options,
                    &repo.join("pkg/src/main.rs"),
                    &HashSet::new(),
                    &mut claims
                )
                .is_empty()
        );
    }

    #[test]
    fn the_upward_walk_skips_already_read_paths() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = root.path().join("repo");
        write(&repo.join("pkg/AGENTS.md"), "pkg rules");
        write(&repo.join("pkg/src/main.rs"), "fn main() {}");

        let options = options_for(root.path(), repo.clone(), Vec::new());
        let system = Instructions::discover(&options);
        let already: HashSet<PathBuf> = [resolve(&repo.join("pkg/AGENTS.md"))].into();
        let mut claims = UpwardClaims::new();
        assert!(
            system
                .nearby(
                    &options,
                    &repo.join("pkg/src/main.rs"),
                    &already,
                    &mut claims
                )
                .is_empty()
        );
    }

    /// The walk never attaches the very file being read, and never leaves the
    /// session directory.
    #[test]
    fn the_upward_walk_is_bounded_by_the_directory_and_skips_the_target() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = root.path().join("repo");
        write(&root.path().join("AGENTS.md"), "outside");
        write(&repo.join("pkg/AGENTS.md"), "pkg rules");

        let options = options_for(root.path(), repo.clone(), Vec::new());
        let system = Instructions::discover(&options);
        let mut claims = UpwardClaims::new();
        let found = system.nearby(
            &options,
            &repo.join("pkg/AGENTS.md"),
            &HashSet::new(),
            &mut claims,
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_deep_read_attaches_every_intermediate_level_once() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = root.path().join("repo");
        write(&repo.join("a/AGENTS.md"), "a");
        write(&repo.join("a/b/AGENTS.md"), "b");
        write(&repo.join("a/b/c/note.txt"), "note");

        let options = options_for(root.path(), repo.clone(), Vec::new());
        let system = Instructions::discover(&options);
        let mut claims = UpwardClaims::new();
        let found = system.nearby(
            &options,
            &repo.join("a/b/c/note.txt"),
            &HashSet::new(),
            &mut claims,
        );
        let paths: Vec<&Path> = found.iter().map(InstructionPath::path).collect();
        assert_eq!(
            paths,
            vec![
                resolve(&repo.join("a/b/AGENTS.md")).as_path(),
                resolve(&repo.join("a/AGENTS.md")).as_path(),
            ]
        );
        assert_eq!(claims.len(), 2);
        assert!(!claims.is_empty());
    }

    #[test]
    fn warning_display_names_the_source_and_the_budget() {
        let timeout = InstructionWarning {
            source: "https://example.invalid/x.md".to_owned(),
            kind: WarningKind::RemoteTimeout,
        };
        assert_eq!(
            timeout.to_string(),
            "instruction https://example.invalid/x.md did not respond within 5s and was skipped"
        );
        assert_eq!(timeout.source(), "https://example.invalid/x.md");
        assert_eq!(timeout.kind(), &WarningKind::RemoteTimeout);

        let status = InstructionWarning {
            source: "https://example.invalid/y.md".to_owned(),
            kind: WarningKind::RemoteStatus(404),
        };
        assert_eq!(
            status.to_string(),
            "instruction https://example.invalid/y.md returned HTTP 404 and was skipped"
        );

        let transport = InstructionWarning {
            source: "https://example.invalid/z.md".to_owned(),
            kind: WarningKind::RemoteTransport("dns error".to_owned()),
        };
        assert!(transport.to_string().contains("dns error"));

        let unreadable = InstructionWarning {
            source: "/repo/AGENTS.md".to_owned(),
            kind: WarningKind::Unreadable(std::io::ErrorKind::PermissionDenied),
        };
        assert!(
            unreadable
                .to_string()
                .starts_with("instruction file /repo/AGENTS.md could not be read")
        );
    }

    #[test]
    fn with_layout_overrides_the_resolved_layout() {
        let root = tempfile::tempdir().expect("tempdir");
        let elsewhere = root.path().join("elsewhere");
        write(&elsewhere.join("zuno/AGENTS.md"), "override global");
        fs::create_dir_all(root.path().join("repo")).expect("mkdir");

        let layout = Layout::resolve(
            &Env::empty()
                .with(
                    HOME,
                    root.path().join("home").to_string_lossy().into_owned(),
                )
                .with(XDG_CONFIG_HOME, elsewhere.to_string_lossy().into_owned()),
        );
        let options =
            options_for(root.path(), root.path().join("repo"), Vec::new()).with_layout(layout);
        let found = Instructions::discover(&options);
        assert_eq!(
            found.paths()[0].path(),
            resolve(&elsewhere.join("zuno/AGENTS.md"))
        );
    }

    #[test]
    fn from_config_reads_the_merged_instructions_list() {
        let root = tempfile::tempdir().expect("tempdir");
        let config = Config {
            instructions: Some(vec!["https://example.invalid/a.md".to_owned()]),
            ..Config::default()
        };
        let options = InstructionOptions::from_config(
            root.path(),
            Some(root.path()),
            &env_for(root.path()),
            &config,
        );
        assert_eq!(Instructions::discover(&options).urls().len(), 1);
        assert_eq!(options.directory(), root.path());
        assert_eq!(options.worktree(), Some(root.path()));
    }

    #[test]
    fn basename_takes_the_last_segment() {
        assert_eq!(basename("/repo/docs/AGENTS.md"), "AGENTS.md");
        assert_eq!(basename("AGENTS.md"), "AGENTS.md");
    }

    #[test]
    fn is_remote_recognizes_both_schemes_only() {
        assert!(is_remote("https://example.invalid/a.md"));
        assert!(is_remote("http://example.invalid/a.md"));
        assert!(!is_remote("ftp://example.invalid/a.md"));
        assert!(!is_remote("~/a.md"));
        assert!(!is_remote("docs/a.md"));
    }
}

//! Instruction-file discovery and the `instructions[]` loader.
//!
//! AGENTS instructions are injected into every prompt, so discovery is native,
//! ordered, and path-de-duplicated. Zuno deliberately has no implicit OpenCode
//! or Claude instruction fallback.
//!
//! # Three mechanisms, deliberately kept apart
//!
//! **1. Global instructions** — only `$XDG_CONFIG_HOME/zuno/AGENTS.md`.
//!
//! **2. Project instructions** — walk from the worktree root to the current
//! directory. In each directory, `AGENTS.local.md` replaces `AGENTS.md`; nearer
//! directories are appended later and therefore have higher priority.
//!
//! **3. Nearby instructions** ([`Instructions::nearby`]) — when a file is read
//! mid-session, walk upward from that file and attach instruction files not
//! already accounted for. The system set, paths already read in the session,
//! and the current message's claims ensure each canonical file is charged once.
//!
//! Configured `instructions[]` paths and URLs remain a separate explicit source.
//! `CLAUDE.md`, `CONTEXT.md`, and other product directories are never loaded
//! implicitly.
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
use zuno_paths::{Env, Layout, node_path};

/// How many local instruction files are read at once (`instruction.ts:157`).
pub const LOCAL_CONCURRENCY: usize = 8;

/// How many remote instruction URLs are fetched at once (`instruction.ts:158`).
pub const REMOTE_CONCURRENCY: usize = 4;

/// The per-URL budget for a remote instruction (`instruction.ts:97`).
pub const REMOTE_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-directory instruction names, highest priority first.
///
/// `AGENTS.local.md` replaces `AGENTS.md` in the same directory. Directories
/// load root-to-current so nearer project rules win later.
pub const INSTRUCTION_FILENAMES: [&str; 2] = ["AGENTS.local.md", "AGENTS.md"];

/// The filename probed inside the global config directory.
pub const GLOBAL_INSTRUCTION_FILENAME: &str = "AGENTS.md";

/// Zuno-owned starter guidance materialized only when the global file is absent.
pub const DEFAULT_GLOBAL_INSTRUCTIONS: &str = include_str!("default-agents.md");

/// The header the oracle puts above every instruction body (`instruction.ts:162`).
const HEADER: &str = "Instructions from: ";

/// Which of the three mechanisms produced a path.
///
/// The oracle collapses all of these into one unordered `Set<string>`. Keeping
/// the provenance costs nothing and is what lets a caller explain to a user why a
/// file is in their prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Origin {
    /// `$XDG_CONFIG_HOME/zuno/AGENTS.md`.
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

    /// Whether `ZUNO_DISABLE_PROJECT_CONFIG` suppressed project discovery.
    #[must_use]
    pub fn project_config_disabled(&self) -> bool {
        self.layout.project_config_disabled()
    }

    fn filenames(&self) -> Vec<&'static str> {
        INSTRUCTION_FILENAMES.to_vec()
    }

    fn global_files(&self) -> Vec<PathBuf> {
        let base = self.layout.config().join(GLOBAL_INSTRUCTION_FILENAME);
        let mut files = vec![base.clone()];
        if let Some(override_dir) = self
            .layout
            .config_dir_override()
            .filter(|value| !value.is_empty())
        {
            let profile = PathBuf::from(override_dir).join(GLOBAL_INSTRUCTION_FILENAME);
            if profile != base {
                files.push(profile);
            }
        }
        files
    }
}

fn project_instruction_files(options: &InstructionOptions) -> Vec<PathBuf> {
    let directory = resolve(options.directory());
    let boundary = options
        .worktree()
        .map(resolve)
        .filter(|root| directory.starts_with(root));
    let mut directories = Vec::new();
    let mut current = directory;
    loop {
        directories.push(current.clone());
        if boundary.as_ref().is_some_and(|root| current == *root) {
            break;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent.to_path_buf();
    }
    directories.reverse();
    directories
        .into_iter()
        .filter_map(|directory| Instructions::find(options, &directory))
        .collect()
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
            }
        }

        if !options.project_config_disabled() {
            for found in project_instruction_files(options) {
                paths.insert(&found, Origin::Project);
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
    use zuno_paths::env::{HOME, XDG_CONFIG_HOME, ZUNO_CONFIG_DIR};

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
        assert_eq!(INSTRUCTION_FILENAMES, ["AGENTS.local.md", "AGENTS.md"]);
        assert!(!INSTRUCTION_FILENAMES.contains(&"CONTEXT.md"));
    }

    #[test]
    fn the_bounds_are_the_oracle_numbers() {
        assert_eq!(LOCAL_CONCURRENCY, 8);
        assert_eq!(REMOTE_CONCURRENCY, 4);
        assert_eq!(REMOTE_TIMEOUT, Duration::from_secs(5));
    }

    #[test]
    fn instruction_names_and_global_probe_are_zuno_only() {
        let root = tempfile::tempdir().expect("tempdir");
        let options = InstructionOptions::new(
            root.path(),
            None::<PathBuf>,
            &env_for(root.path()),
            Vec::new(),
        );
        assert_eq!(options.filenames(), vec!["AGENTS.local.md", "AGENTS.md"]);
        assert_eq!(options.global_files().len(), 1);
    }

    #[test]
    fn the_global_probe_is_zuno_only() {
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
    fn a_profile_appends_rules_without_hiding_the_base_global_agents() {
        let root = tempfile::tempdir().expect("tempdir");
        let base_agents = root.path().join("home/.config/zuno/AGENTS.md");
        let profile = root.path().join("profile");
        let profile_agents = profile.join("AGENTS.md");
        write(&base_agents, "base global agents");
        write(&profile_agents, "profile agents");
        fs::create_dir_all(root.path().join("repo")).expect("mkdir repo");
        let env =
            env_for(root.path()).with(ZUNO_CONFIG_DIR, profile.to_string_lossy().into_owned());
        let options =
            InstructionOptions::new(root.path().join("repo"), None::<PathBuf>, &env, Vec::new());

        let found = Instructions::discover(&options);
        let globals: Vec<_> = found
            .paths()
            .iter()
            .filter(|entry| entry.origin() == Origin::Global)
            .collect();
        assert_eq!(globals.len(), 2);
        assert_eq!(globals[0].path(), resolve(&base_agents));
        assert_eq!(globals[1].path(), resolve(&profile_agents));
    }

    #[test]
    fn a_profile_without_agents_keeps_the_base_global_agents() {
        let root = tempfile::tempdir().expect("tempdir");
        let base_agents = root.path().join("home/.config/zuno/AGENTS.md");
        let profile = root.path().join("profile");
        write(&base_agents, "base global agents");
        fs::create_dir_all(&profile).expect("mkdir profile");
        fs::create_dir_all(root.path().join("repo")).expect("mkdir repo");
        let env =
            env_for(root.path()).with(ZUNO_CONFIG_DIR, profile.to_string_lossy().into_owned());
        let options =
            InstructionOptions::new(root.path().join("repo"), None::<PathBuf>, &env, Vec::new());

        let found = Instructions::discover(&options);
        let globals: Vec<_> = found
            .paths()
            .iter()
            .filter(|entry| entry.origin() == Origin::Global)
            .collect();
        assert_eq!(globals.len(), 1);
        assert_eq!(globals[0].path(), resolve(&base_agents));
    }

    #[test]
    fn claude_global_is_never_loaded_implicitly() {
        let root = tempfile::tempdir().expect("tempdir");
        let claude = root.path().join("home/.claude/CLAUDE.md");
        write(&claude, "global claude");
        fs::create_dir_all(root.path().join("repo")).expect("mkdir repo");

        let found = Instructions::discover(&options_for(
            root.path(),
            root.path().join("repo"),
            Vec::new(),
        ));
        assert!(found.paths().is_empty());
    }

    /// Project rules load root-to-current, with local replacing base in one directory.
    #[test]
    fn project_rules_are_ordered_by_scope_and_local_overrides_the_same_directory() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = root.path().join("repo");
        write(&repo.join("AGENTS.md"), "root agents");
        write(&repo.join("sub/AGENTS.md"), "sub agents");
        write(&repo.join("sub/AGENTS.local.md"), "sub local");

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
                resolve(&repo.join("AGENTS.md")).as_path(),
                resolve(&repo.join("sub/AGENTS.local.md")).as_path(),
            ]
        );
    }

    /// Claude instruction files never participate in the project cascade.
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
    fn claude_md_is_ignored_when_no_level_has_agents_md() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = root.path().join("repo");
        write(&repo.join("CLAUDE.md"), "root claude");
        write(&repo.join("sub/CLAUDE.md"), "sub claude");

        let found = Instructions::discover(&options_for(root.path(), repo.join("sub"), Vec::new()));
        assert!(found.paths().is_empty());
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
        write(&repo.join("sub/AGENTS.md"), "sub agents");
        write(&repo.join("sub/AGENTS.local.md"), "sub local");
        write(&repo.join("other/CLAUDE.md"), "other claude");

        let options = options_for(root.path(), repo.clone(), Vec::new());
        assert_eq!(
            Instructions::find(&options, &repo.join("sub")),
            Some(resolve(&repo.join("sub/AGENTS.local.md")))
        );
        assert_eq!(Instructions::find(&options, &repo.join("other")), None);
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

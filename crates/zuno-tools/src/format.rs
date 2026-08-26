//! Running the configured formatters after a successful write.
//!
//! Todo 18 parsed the `formatter` config and todo 39 left a named seam
//! ([`crate::FileFormatter`]) where execution belongs. This module is what goes
//! behind that seam: it decides which formatters claim a path, runs them, and —
//! the part that matters most — guarantees that a formatter which fails cannot
//! cost the caller its edit.
//!
//! Oracle: `packages/opencode/src/format/index.ts` for detection and invocation,
//! `packages/opencode/src/format/formatter.ts` for the built-in table (ported as
//! data in [`builtin`]), `packages/core/src/v1/config/formatter.ts:5-13` for the
//! config surface, which reaches here already resolved as
//! [`zuno_catalog::formatter::ResolvedFormatters`].
//!
//! # A formatter failure must never lose the edit
//!
//! The order is fixed: the bytes are written by the file tool, *then* a formatter
//! is offered the file. So by the time anything can go wrong the edit is already
//! durable, and the only way to lose it is for the formatter itself to damage the
//! file. That is not hypothetical — a formatter killed at a timeout, or one that
//! truncates before failing to parse, leaves a file that is neither the edit nor a
//! formatted version of it.
//!
//! So [`Formatters`] keeps the post-write bytes and **restores them whenever the
//! formatter did not succeed** — a non-zero exit, a spawn failure, or a timeout.
//! The contract a caller can rely on is therefore exact: *after a failed format the
//! file holds precisely the bytes the edit wrote*, and the failure is reported
//! rather than swallowed.
//!
//! This is a deliberate divergence. `format/index.ts:96-113` logs the failure and
//! keeps whatever the formatter left on disk. That costs an edit in the truncation
//! case, and it also means the same input can produce different files depending on
//! how far the formatter got. The price of restoring is that a formatter which
//! exits non-zero *after* doing useful work — `rubocop --autocorrect` returns 1
//! while any offence remains uncorrected — has its partial work discarded, leaving
//! the write formatted exactly as much as it would be if rubocop were not
//! installed. Losing an optional partial reformat is a much smaller harm than
//! keeping a mangled file, and it is the harm the caller was told to expect.
//!
//! # Formatting happens in place, not through a temporary file
//!
//! Every command in the built-in table mutates the file it is given: `-w`, `-i`,
//! `--write`, `--fix`. Handing them a temp copy would break the ones that resolve
//! configuration relative to the file's own directory — `.clang-format`,
//! `.ocamlformat`, `rustfmt.toml`, `biome.json` are all found by walking up from
//! the target — and the ones that key behaviour on the filename. A temp file would
//! buy atomicity against a concurrent reader; restoring the pre-format bytes buys
//! the property that actually matters, which is that the *edit* survives, and it
//! buys it without lying to the formatter about where the file lives.
//!
//! # Why a formatter does not go through the risk gate
//!
//! [`crate::risk`] exists because `shell` executes a string the **model** composed.
//! A formatter command comes from either this module's compile-time table or the
//! operator's config, so there is no model-authored text anywhere in it and the
//! gate has no audience. It is also spawned as argv with no shell, so there is no
//! command string to parse in the first place — `rm -rf` cannot appear as a side
//! effect of word splitting when there is no word splitting. What is borrowed from
//! [`crate::shell`] is the hygiene rather than the policy: `stdin` closed, both
//! output streams captured, `kill_on_drop`, a process group on Unix, and a hard
//! ceiling.

pub mod builtin;

pub use builtin::{Availability, DEFINITIONS, Definition, Environment};

use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use zuno_catalog::formatter::{ResolvedFormatter, ResolvedFormatters};

/// How long one formatter may run before it is abandoned.
///
/// The oracle has no ceiling at all, so a hung formatter hangs the edit forever.
/// Thirty seconds is far more than any formatter in the table needs for one file
/// and far less than a user will wait before assuming the tool is broken.
pub const CEILING: Duration = Duration::from_secs(30);

/// How much of a failing formatter's stderr is carried back.
///
/// Enough for a compiler-style diagnostic with context; short enough that a
/// formatter looping on output cannot flood the turn.
pub const MAX_STDERR_BYTES: usize = 4_096;

/// The oracle's placeholder for the file being formatted (`format/index.ts:79`).
pub const FILE_PLACEHOLDER: &str = "$FILE";

/// The metadata key carrying formatter failures back to the caller.
pub const METADATA_FAILURES_KEY: &str = "formatterFailures";

/// Why a formatter did not complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The process could not be started at all.
    NotSpawned,
    /// The process ran and reported failure.
    Exited {
        /// The exit status, absent when the process was killed by a signal.
        code: Option<i32>,
    },
    /// The process outlived the ceiling and was abandoned.
    TimedOut {
        /// The ceiling it outlived, in seconds.
        after_seconds: u64,
    },
}

impl fmt::Display for FailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSpawned => f.write_str("could not be started"),
            Self::Exited { code: Some(code) } => write!(f, "exited with status {code}"),
            Self::Exited { code: None } => f.write_str("was killed by a signal"),
            Self::TimedOut { after_seconds } => {
                write!(f, "did not finish within {after_seconds}s")
            }
        }
    }
}

/// One formatter's failure, with the evidence a human needs to fix it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatFailure {
    /// The formatter's configured name.
    pub formatter: String,
    /// The argv that was run, with `$FILE` already substituted.
    pub command: Vec<String>,
    /// What went wrong.
    pub kind: FailureKind,
    /// The formatter's stderr, capped at [`MAX_STDERR_BYTES`]. This is the part
    /// that says *why*, so it is carried verbatim rather than summarised.
    pub stderr: String,
    /// Whether the file had to be rewritten to undo damage the formatter did.
    ///
    /// `false` is the common case: most formatters that fail leave the file alone.
    /// `true` says the edit was recovered, which is worth reporting because it
    /// means this formatter is actively destructive on this input.
    pub restored: bool,
}

impl FormatFailure {
    /// The metadata shape a caller attaches under [`METADATA_FAILURES_KEY`].
    #[must_use]
    pub fn to_metadata(&self) -> Value {
        let mut entry = Map::new();
        entry.insert("formatter".to_owned(), json!(self.formatter));
        entry.insert("command".to_owned(), json!(self.command));
        entry.insert("reason".to_owned(), json!(self.kind.to_string()));
        if let FailureKind::Exited { code: Some(code) } = self.kind {
            entry.insert("exitCode".to_owned(), json!(code));
        }
        entry.insert("stderr".to_owned(), json!(self.stderr));
        entry.insert("editRestored".to_owned(), json!(self.restored));
        Value::Object(entry)
    }
}

impl fmt::Display for FormatFailure {
    /// One line the model can act on: which formatter, what happened, its stderr.
    ///
    /// The edit's survival is stated explicitly. Without it a model reading
    /// "formatter failed" has every reason to assume its write was lost and to redo
    /// it, which is the wasteful behaviour this whole module exists to avoid.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Formatter `{}` {} ({}). The edit was written and is intact",
            self.formatter,
            self.kind,
            self.command.join(" ")
        )?;
        if self.restored {
            f.write_str("; the formatter's damage to the file was undone")?;
        }
        f.write_str(".")?;
        if !self.stderr.trim().is_empty() {
            write!(f, "\n{}", self.stderr.trim_end())?;
        }
        Ok(())
    }
}

/// What one format pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FormatOutcome {
    /// Whether any formatter changed the file's bytes.
    pub changed: bool,
    /// Formatters that were offered the file and failed, in the order they ran.
    ///
    /// A non-empty list is **not** an error: the write succeeded, so this is
    /// diagnostic payload rather than a failed operation. Modelling it as an `Err`
    /// would make the tool report a failure for an edit that is on disk.
    pub failures: Vec<FormatFailure>,
}

impl FormatOutcome {
    /// Whether anything needs reporting.
    #[must_use]
    pub fn has_failures(&self) -> bool {
        !self.failures.is_empty()
    }

    /// The failures rendered for the model, one per line.
    #[must_use]
    pub fn report(&self) -> String {
        self.failures
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The failures as metadata, or `None` when there were none.
    #[must_use]
    pub fn failure_metadata(&self) -> Option<Value> {
        if self.failures.is_empty() {
            return None;
        }
        Some(Value::Array(
            self.failures
                .iter()
                .map(FormatFailure::to_metadata)
                .collect(),
        ))
    }
}

/// Where a program is found.
///
/// Injectable because the alternative is a test that mutates `PATH`, and mutating
/// the environment is `unsafe` and forbidden in this workspace. A stub locator also
/// lets a test exercise a *built-in* definition without the machine having that
/// formatter installed, and without this crate ever installing one.
pub trait ProgramLocator: Send + Sync + fmt::Debug {
    /// The absolute path of `program`, or `None` when it is not on `PATH`.
    fn locate(&self, program: &str) -> Option<PathBuf>;
}

/// `PATH` lookup, matching the oracle's `which()`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemPrograms;

impl ProgramLocator for SystemPrograms {
    fn locate(&self, program: &str) -> Option<PathBuf> {
        which::which(program).ok()
    }
}

/// A formatter as it will actually be run, with the config union already applied.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    name: String,
    command: Vec<String>,
    extensions: Vec<String>,
    environment: BTreeMap<String, String>,
    /// `None` means the command came from config, which the oracle treats as
    /// unconditionally available: `format/index.ts:154` replaces `enabled` with
    /// `async () => info.command ?? false` whenever an override supplies one.
    availability: Option<Availability>,
    experimental: bool,
    shadowed_by: Option<String>,
}

impl Entry {
    fn from_builtin(definition: &Definition) -> Self {
        Self {
            name: definition.name.to_owned(),
            command: definition
                .command
                .iter()
                .map(|&argument| argument.to_owned())
                .collect(),
            extensions: definition
                .extensions
                .iter()
                .map(|&extension| extension.to_owned())
                .collect(),
            environment: definition
                .environment
                .iter()
                .map(|&(key, value)| (key.to_owned(), value.to_owned()))
                .collect(),
            availability: Some(definition.availability),
            experimental: definition.experimental,
            shadowed_by: definition.shadowed_by.map(str::to_owned),
        }
    }

    /// A config entry naming no built-in, which declares a formatter of its own.
    ///
    /// `extensions: info.extensions ?? []` (`format/index.ts:156`) means such an
    /// entry with no extensions claims nothing and can never run — the correct
    /// reading of a config entry that forgot to say what it formats.
    fn from_override(over: &ResolvedFormatter) -> Self {
        Self {
            name: over.name.clone(),
            command: over.command.clone().unwrap_or_default(),
            extensions: over.extensions.clone().unwrap_or_default(),
            environment: over
                .environment
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            availability: None,
            experimental: false,
            shadowed_by: None,
        }
    }

    /// Apply one config override to a built-in, following
    /// `format/index.ts:145-157`.
    ///
    /// `mergeDeep(builtIn, item)` there means every field the override omits keeps
    /// the built-in's value, which is why each field is conditional.
    fn apply(&mut self, over: &ResolvedFormatter) {
        if let Some(command) = &over.command {
            self.command = command.clone();
            // A configured command replaces the availability probe outright rather
            // than being tested against it: an operator naming a command is the
            // assertion that it exists.
            self.availability = None;
            self.shadowed_by = None;
        }
        if let Some(extensions) = &over.extensions {
            self.extensions = extensions.clone();
        }
        for (key, value) in &over.environment {
            self.environment.insert(key.clone(), value.clone());
        }
    }

    fn claims(&self, extension: &str) -> bool {
        self.extensions.iter().any(|entry| entry == extension)
    }

    fn program(&self) -> &str {
        self.command.first().map_or("", String::as_str)
    }
}

/// The post-write formatter runtime.
///
/// Built once per instance from the resolved config and reused: the registry is a
/// projection of config plus the built-in table, neither of which changes while a
/// session runs.
#[derive(Debug, Clone)]
pub struct Formatters {
    /// Where `findUp` starts and where a formatter is spawned, matching the
    /// oracle's `InstanceState.directory` (`format/index.ts:85`).
    directory: PathBuf,
    /// Where `findUp` stops (`Filesystem.findUp`'s `stop` argument).
    worktree: PathBuf,
    entries: Vec<Entry>,
    experimental_oxfmt: bool,
    locator: Arc<dyn ProgramLocator>,
    ceiling: Duration,
}

impl Formatters {
    /// Build the registry from resolved config.
    ///
    /// An entirely disabled `formatter` key yields a registry with no entries, so
    /// [`Formatters::format_all`] short-circuits without touching the filesystem —
    /// the same effect as `format/index.ts:120-128` returning before the built-ins
    /// are ever registered.
    #[must_use]
    pub fn new(directory: &Path, worktree: &Path, resolved: &ResolvedFormatters) -> Self {
        Self {
            directory: directory.to_path_buf(),
            worktree: worktree.to_path_buf(),
            entries: Self::registry(resolved),
            experimental_oxfmt: false,
            locator: Arc::new(SystemPrograms),
            ceiling: CEILING,
        }
    }

    /// Replace the program locator: for tests, and for a host that resolves tools
    /// somewhere other than `PATH`.
    #[must_use]
    pub fn with_locator(mut self, locator: Arc<dyn ProgramLocator>) -> Self {
        self.locator = locator;
        self
    }

    /// Set the `experimentalOxfmt` runtime flag (`format/formatter.ts:96`).
    #[must_use]
    pub const fn with_experimental_oxfmt(mut self, enabled: bool) -> Self {
        self.experimental_oxfmt = enabled;
        self
    }

    /// Shorten the per-formatter ceiling, for a test that must observe a timeout.
    #[must_use]
    pub const fn with_ceiling(mut self, ceiling: Duration) -> Self {
        self.ceiling = ceiling;
        self
    }

    fn registry(resolved: &ResolvedFormatters) -> Vec<Entry> {
        if !resolved.is_enabled() {
            return Vec::new();
        }
        let mut entries: Vec<Entry> = DEFINITIONS
            .iter()
            .filter(|definition| resolved.is_formatter_enabled(definition.name))
            .map(Entry::from_builtin)
            .collect();
        for over in resolved.overrides() {
            match entries.iter_mut().find(|entry| entry.name == over.name) {
                Some(entry) => entry.apply(over),
                None => entries.push(Entry::from_override(over)),
            }
        }
        entries
    }

    /// The names in the registry, in the order they would run.
    ///
    /// Exposed so a caller — and a test — can see the effect of the config without
    /// having to format a file to find out.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| entry.name.as_str())
    }

    /// Whether `name` survived the config.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.entries.iter().any(|entry| entry.name == name)
    }

    /// The names that claim `extension`, in the order they would run.
    pub fn claiming(&self, extension: &str) -> impl Iterator<Item = &str> {
        self.entries
            .iter()
            .filter(move |entry| entry.claims(extension))
            .map(|entry| entry.name.as_str())
    }

    /// The extension used for matching, spelled the way the oracle spells it.
    ///
    /// `path.extname()` returns a leading dot and only the final segment, so
    /// `a.html.erb` is `.erb` — which is why `htmlbeautifier`'s `.html.erb` entry
    /// (`format/formatter.ts:271`) can never match upstream either. It is carried
    /// anyway; silently correcting the oracle's table would be a divergence hiding
    /// inside a port.
    #[must_use]
    pub fn extension_of(path: &Path) -> String {
        path.extension()
            .and_then(|extension| extension.to_str())
            .map_or_else(String::new, |extension| format!(".{extension}"))
    }

    /// Format `path` with every formatter that claims it.
    ///
    /// Never returns an error. Every way this can go wrong is a diagnostic about a
    /// formatter, not a failure of the write that already happened, so it all comes
    /// back in [`FormatOutcome::failures`].
    pub async fn format_all(&self, path: &Path) -> FormatOutcome {
        let mut outcome = FormatOutcome::default();
        if self.entries.is_empty() {
            return outcome;
        }
        let extension = Self::extension_of(path);
        // No extension claims nothing, and matching on an empty string would let a
        // config entry with `extensions: [""]` claim every extensionless file.
        if extension.is_empty() {
            return outcome;
        }
        let claimants: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|entry| entry.claims(&extension))
            .collect();

        for entry in claimants {
            let Some(argv) = self.resolve(entry).await else {
                continue;
            };
            let Ok(before) = std::fs::read(path) else {
                // The file vanished between the write and the format. There is
                // nothing to format and nothing to protect; the caller's own re-read
                // reports it properly.
                return outcome;
            };
            let command = substitute(&argv, path);
            match self.spawn_formatter(entry, &command).await {
                Ok(()) => {
                    if std::fs::read(path).is_ok_and(|after| after != before) {
                        outcome.changed = true;
                    }
                }
                Err((kind, stderr)) => {
                    let restored = restore(path, &before);
                    outcome.failures.push(FormatFailure {
                        formatter: entry.name.clone(),
                        command,
                        kind,
                        stderr,
                        restored,
                    });
                }
            }
        }
        outcome
    }

    /// The argv this formatter would run, or `None` when it is unavailable.
    ///
    /// The shadow check is here rather than in [`Formatters::probe`] so the two
    /// cannot recurse: `uv` stands down for `ruff`, and asking whether `ruff` is
    /// available must not ask whether anything shadows *it*.
    async fn resolve(&self, entry: &Entry) -> Option<Vec<String>> {
        if let Some(winner) = &entry.shadowed_by
            && let Some(other) = self.entries.iter().find(|item| &item.name == winner)
            && self.probe(other).await.is_some()
        {
            return None;
        }
        self.probe(entry).await
    }

    /// Mirrors `Info.enabled(context)` returning `string[] | false`.
    ///
    /// The oracle caches the answer per formatter (`format/index.ts:42-48`) but the
    /// cache is only consulted for a *positive* result — `cmd === false` re-probes —
    /// so nothing observable is lost by not caching, and an operator installing a
    /// formatter mid-session is picked up.
    async fn probe(&self, entry: &Entry) -> Option<Vec<String>> {
        if entry.command.is_empty() {
            return None;
        }
        let Some(availability) = entry.availability else {
            return Some(entry.command.clone());
        };
        if entry.experimental && !self.experimental_oxfmt {
            return None;
        }
        match availability {
            Availability::Program => self.with_program(entry),
            Availability::ProgramWithMarker(markers) => {
                if self.find_up(markers).next().is_some() {
                    self.with_program(entry)
                } else {
                    None
                }
            }
            Availability::ProgramWithHelpFirstLine(fragments) => {
                let argv = self.with_program(entry)?;
                let first = self.help_first_line(&argv[0]).await?;
                fragments
                    .iter()
                    .all(|fragment| first.contains(fragment))
                    .then_some(argv)
            }
            Availability::ProgramWithHelpExitZero => {
                let argv = self.with_program(entry)?;
                self.help_first_line(&argv[0]).await.map(|_| argv)
            }
            Availability::NodeMarker(markers) => {
                if self.find_up(markers).next().is_some() {
                    self.with_node_bin(entry)
                } else {
                    None
                }
            }
            Availability::NodePackage {
                manifest,
                keys,
                package,
            } => {
                if self.declares(manifest, keys, package) {
                    self.with_node_bin(entry)
                } else {
                    None
                }
            }
            Availability::VendoredPackage {
                manifest,
                keys,
                package,
            } => {
                // The command is already the vendored path and is spawned with the
                // instance directory as cwd, exactly as upstream does.
                if self.declares(manifest, keys, package) {
                    Some(entry.command.clone())
                } else {
                    None
                }
            }
            Availability::RuffConfig => {
                if self.ruff_configured() {
                    self.with_program(entry)
                } else {
                    None
                }
            }
        }
    }

    /// The argv with its program replaced by the absolute path `which` found.
    fn with_program(&self, entry: &Entry) -> Option<Vec<String>> {
        let located = self.locator.locate(entry.program())?;
        let mut argv = entry.command.clone();
        argv[0] = located.to_string_lossy().into_owned();
        Some(argv)
    }

    /// The argv with its program replaced by a `node_modules/.bin` entry.
    ///
    /// The oracle uses `Npm.which`, which resolves from a global per-package install
    /// directory **and installs the package when it is missing**
    /// (`core/src/npm.ts:192-241`). Installing a package to satisfy a format is out
    /// of scope here, so this looks for the project's own `node_modules/.bin`
    /// instead: a project that has the formatter installed still formats, and one
    /// that does not is skipped rather than triggering a download.
    fn with_node_bin(&self, entry: &Entry) -> Option<Vec<String>> {
        let relative = Path::new("node_modules").join(".bin").join(entry.program());
        let located = self
            .directories()
            .map(|directory| directory.join(&relative))
            .find(|candidate| candidate.is_file())?;
        let mut argv = entry.command.clone();
        argv[0] = located.to_string_lossy().into_owned();
        Some(argv)
    }

    /// The directories `findUp` walks: the instance directory, then its ancestors,
    /// with the worktree included and nothing above it
    /// (`util/filesystem.ts:192-200` — `stop` is compared before the parent is
    /// pushed, so `stop` itself is in the list).
    fn directories(&self) -> impl Iterator<Item = PathBuf> + '_ {
        let mut current = Some(self.directory.clone());
        std::iter::from_fn(move || {
            let directory = current.take()?;
            current = if directory == self.worktree {
                None
            } else {
                directory.parent().map(Path::to_path_buf)
            };
            Some(directory)
        })
    }

    /// Every existing `<dir>/<marker>` walking up, nearest first.
    fn find_up<'a>(&'a self, markers: &'a [&'a str]) -> impl Iterator<Item = PathBuf> + 'a {
        self.directories().flat_map(move |directory| {
            markers
                .iter()
                .map(|marker| directory.join(marker))
                .filter(|candidate| candidate.is_file())
                .collect::<Vec<_>>()
        })
    }

    /// Whether any `manifest` found walking up declares `package` under one of
    /// `keys`.
    ///
    /// The oracle reads **every** manifest it finds and returns on the first that
    /// declares the package (`format/formatter.ts:70-80`), rather than stopping at
    /// the nearest one, so this does the same.
    fn declares(&self, manifest: &str, keys: &[&str], package: &str) -> bool {
        self.find_up(&[manifest]).any(|path| {
            let Ok(raw) = std::fs::read_to_string(&path) else {
                return false;
            };
            let Ok(Value::Object(json)) = serde_json::from_str::<Value>(&raw) else {
                return false;
            };
            keys.iter().any(|key| {
                json.get(*key)
                    .and_then(Value::as_object)
                    .is_some_and(|map| map.contains_key(package))
            })
        })
    }

    /// Ruff's layered check (`format/formatter.ts:189-216`).
    fn ruff_configured(&self) -> bool {
        for config in ["pyproject.toml", "ruff.toml", ".ruff.toml"] {
            let Some(found) = self.find_up(&[config]).next() else {
                continue;
            };
            if config != "pyproject.toml" {
                return true;
            }
            if std::fs::read_to_string(&found).is_ok_and(|raw| raw.contains("[tool.ruff]")) {
                return true;
            }
        }
        // Falling back to "some dependency file mentions ruff" is upstream's rule,
        // substring match included. It is loose — a comment naming ruff counts — but
        // it is the rule, and tightening it here would silently stop formatting
        // projects the oracle formats.
        ["requirements.txt", "pyproject.toml", "Pipfile"]
            .iter()
            .any(|dependency| {
                self.find_up(&[dependency])
                    .next()
                    .and_then(|path| std::fs::read_to_string(path).ok())
                    .is_some_and(|raw| raw.contains("ruff"))
            })
    }

    /// The first line of `program --help`, or `None` when it did not exit zero.
    async fn help_first_line(&self, program: &str) -> Option<String> {
        let mut process = Command::new(program);
        process
            .arg("--help")
            .current_dir(&self.directory)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        process.process_group(0);
        let child = process.spawn().ok()?;
        let output = tokio::time::timeout(self.ceiling, child.wait_with_output())
            .await
            .ok()?
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        Some(text.lines().next().unwrap_or_default().to_owned())
    }

    /// Run one formatter. `Err` carries what to report.
    async fn spawn_formatter(
        &self,
        entry: &Entry,
        command: &[String],
    ) -> Result<(), (FailureKind, String)> {
        let Some((program, arguments)) = command.split_first() else {
            return Err((FailureKind::NotSpawned, "empty command".to_owned()));
        };
        let mut process = Command::new(program);
        process
            .args(arguments)
            .current_dir(&self.directory)
            .envs(&entry.environment)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // A dedicated group so a formatter that spawns helpers is torn down with
        // them, for the reason `shell.rs` does the same.
        #[cfg(unix)]
        process.process_group(0);

        let child = match process.spawn() {
            Ok(child) => child,
            Err(error) => return Err((FailureKind::NotSpawned, error.to_string())),
        };
        // `kill_on_drop` terminates the child when this future is dropped at the
        // timeout, which is why the ceiling needs no separate kill path.
        let output = match tokio::time::timeout(self.ceiling, child.wait_with_output()).await {
            Err(_) => {
                return Err((
                    FailureKind::TimedOut {
                        after_seconds: self.ceiling.as_secs(),
                    },
                    String::new(),
                ));
            }
            Ok(Err(error)) => return Err((FailureKind::NotSpawned, error.to_string())),
            Ok(Ok(output)) => output,
        };
        if output.status.success() {
            return Ok(());
        }
        Err((
            FailureKind::Exited {
                code: output.status.code(),
            },
            cap(&output.stderr),
        ))
    }
}

/// Put `path` where the placeholder is, in every argument that holds it.
///
/// Substituting in every argument rather than only the last matches
/// `format/index.ts:80`, and it is what lets a configured command put the file
/// anywhere in its argv.
fn substitute(argv: &[String], path: &Path) -> Vec<String> {
    let rendered = path.to_string_lossy();
    argv.iter()
        .map(|argument| argument.replace(FILE_PLACEHOLDER, &rendered))
        .collect()
}

/// Put `before` back, reporting whether that was necessary.
///
/// A failed rewrite here would be a genuine loss, but there is nothing useful to do
/// about it at this layer: the caller re-reads the file and records its real bytes,
/// so a damaged file is reported as content rather than as a formatter diagnostic.
/// Returning `false` keeps the report honest — it says the damage was *not* undone.
fn restore(path: &Path, before: &[u8]) -> bool {
    match std::fs::read(path) {
        Ok(current) if current == before => false,
        _ => std::fs::write(path, before).is_ok(),
    }
}

#[async_trait::async_trait]
impl crate::FileFormatter for Formatters {
    /// The narrow seam: whether the bytes changed, with failures dropped.
    ///
    /// A caller that wants the diagnostics uses
    /// [`crate::FileFormatter::format_reporting`]. `Ok` even when a formatter
    /// failed, because the write it followed did not.
    async fn format(&self, path: &Path) -> std::io::Result<bool> {
        Ok(self.format_all(path).await.changed)
    }

    async fn format_reporting(&self, path: &Path) -> std::io::Result<FormatOutcome> {
        Ok(self.format_all(path).await)
    }
}

/// Cap stderr at [`MAX_STDERR_BYTES`], on a character boundary.
fn cap(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= MAX_STDERR_BYTES {
        return text.into_owned();
    }
    let mut end = MAX_STDERR_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n(stderr truncated)", &text[..end])
}

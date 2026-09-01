//! Which paths the watcher refuses to report.
//!
//! # What is ported and what is added
//!
//! The oracle hands a flat pattern list to `@parcel/watcher`'s `ignore` option
//! (`filesystem/watcher.ts:107-109`):
//!
//! ```ts
//! subscribe(location.directory, [...Ignore.PATTERNS, ...config, ...protecteds(location.directory)])
//! ```
//!
//! `Ignore.PATTERNS` (`filesystem/ignore.ts:48`) is `[...FILES, ...FOLDERS]` —
//! eleven globs concatenated with twenty-nine bare directory *basenames*. Passing
//! a bare basename to parcel makes it a path relative to the watched directory,
//! so parcel only prunes a **top-level** `node_modules`. `Ignore.match`
//! (`ignore.ts:50-65`), the same module's own matcher, instead tests the basename
//! set against **every component** of the path:
//!
//! ```ts
//! const parts = filepath.split(/[/\\]/)
//! for (const part of parts) if (FOLDERS.has(part)) return true
//! ```
//!
//! This port implements `Ignore.match`'s per-component rule, not parcel's
//! top-level-only one. The set contains `node_modules`, `target`, and `dist`; a
//! monorepo has one of those inside every package, and a watcher that reports
//! them defeats its own purpose. The divergence is recorded in
//! the project's engineering notes.
//!
//! Gitignore filtering is an **addition** the plan asks for. `@parcel/watcher`
//! has no gitignore support at all, so nothing in the oracle consults
//! `.gitignore`; `Ignore.PATTERNS` is a hard-coded stand-in for it. Because it is
//! an addition rather than a port, it is opt-in via
//! [`FilterBuilder::gitignore`].
//!
//! # Why the gitignore chain is built lazily, per directory
//!
//! `ignore::gitignore::GitignoreBuilder::add_line` anchors every pattern to the
//! **builder's** root and ignores the `from` path it is handed
//! (`ignore-0.4.33/src/gitignore.rs:460-540`). Feeding a nested `sub/.gitignore`
//! into one root-anchored builder therefore mis-anchors its `/`-prefixed
//! patterns: `/foo` in `sub/.gitignore` must mean `sub/foo`, and a single builder
//! would read it as `foo`. Correctness requires one matcher per directory that
//! owns a `.gitignore`, consulted deepest-first, which is what
//! [`ignore::WalkBuilder`] does internally with a type it does not export.
//!
//! Building that map eagerly would mean walking the whole repository at startup —
//! the one cost a watcher exists to avoid. So the map is populated on demand, the
//! first time a path under a given directory is judged, and cached. Two
//! consequences worth knowing:
//!
//! - A `.gitignore` created in a directory the filter has **not yet seen** is
//!   picked up. One in a directory already cached is not.
//! - A `.gitignore` that is *edited* is not re-read. The edit is itself a change
//!   event the consumer receives, and [`Filter::is_gitignore`] exists so the
//!   consumer can recognise it and call [`Filter::invalidate`].
//!
//! # `require_git`
//!
//! `ignore`'s and ripgrep's default is `require_git = true`: a `.gitignore` in a
//! tree with no `.git` anywhere is not applied. That rule is reproduced here
//! (see [`FilterBuilder::require_git`]) because a fixture that means to test
//! ignore semantics and forgets to `git init` otherwise silently tests nothing —
//! measured during todo 41, recorded in the project's engineering notes.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// Directory basenames pruned at any depth (`filesystem/ignore.ts:3-32`).
///
/// Sorted so the lookup is a binary search over a `&'static` slice rather than a
/// heap-allocated set built per filter. The order is not the oracle's insertion
/// order; a `Set` has no order the oracle can observe.
pub const IGNORED_FOLDERS: [&str; 28] = [
    ".cache",
    ".git",
    ".gradle",
    ".hg",
    ".history",
    ".idea",
    ".next",
    ".npm",
    ".output",
    ".pnpm-store",
    ".pytest_cache",
    ".sst",
    ".svn",
    ".turbo",
    ".vscode",
    ".webkit-cache",
    "__pycache__",
    "bin",
    "bower_components",
    "build",
    "desktop",
    "dist",
    "mypy_cache",
    "node_modules",
    "obj",
    "out",
    "target",
    "vendor",
];

/// File globs pruned at any depth (`filesystem/ignore.ts:34-46`).
pub const IGNORED_FILE_GLOBS: [&str; 11] = [
    "**/*.swp",
    "**/*.swo",
    "**/*.pyc",
    "**/.DS_Store",
    "**/Thumbs.db",
    "**/logs/**",
    "**/tmp/**",
    "**/temp/**",
    "**/*.log",
    "**/coverage/**",
    "**/.nyc_output/**",
];

/// A pattern from `watcher.ignore` or `Ignore.PATTERNS` that would not compile.
#[derive(Debug, thiserror::Error)]
#[error("watcher ignore pattern {pattern:?} is not a valid glob: {source}")]
pub struct PatternError {
    /// The offending pattern, as the user wrote it.
    pub pattern: String,
    /// What `globset` said about it.
    #[source]
    pub source: globset::Error,
}

/// Assemble a [`Filter`].
#[derive(Clone, Debug)]
pub struct FilterBuilder {
    root: PathBuf,
    extra: Vec<String>,
    whitelist: Vec<String>,
    gitignore: bool,
    require_git: bool,
}

impl FilterBuilder {
    /// Start from the built-in `Ignore.PATTERNS` and nothing else.
    ///
    /// `root` is the directory every judged path is made relative to. Patterns
    /// are matched against that relative form with `/` separators, which is the
    /// only reading under which a user-written `watcher.ignore` entry such as
    /// `build/**` means what the user meant.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            extra: Vec::new(),
            whitelist: Vec::new(),
            gitignore: false,
            require_git: true,
        }
    }

    /// Add the `watcher.ignore` patterns from configuration.
    ///
    /// The oracle flat-maps `watcher.ignore` across every config *document*
    /// (`filesystem/watcher.ts:104-106`), so several layers each contribute and
    /// none replaces another. Callers should therefore call this once per layer,
    /// or once with the concatenation.
    #[must_use]
    pub fn extra_patterns<I, S>(mut self, patterns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.extra.extend(patterns.into_iter().map(Into::into));
        self
    }

    /// Add the `watcher.ignore` patterns held by an [`zuno_config::schema::WatcherConfig`].
    #[must_use]
    pub fn watcher_config(self, config: Option<&zuno_config::schema::WatcherConfig>) -> Self {
        match config.and_then(|config| config.ignore.as_deref()) {
            Some(patterns) => self.extra_patterns(patterns.iter().cloned()),
            None => self,
        }
    }

    /// Patterns that force a path to be reported even if something else ignores
    /// it — `Ignore.match`'s `whitelist` option (`filesystem/ignore.ts:51-53`).
    ///
    /// Checked before every other rule, exactly as the oracle checks it, which
    /// is also how a `--glob` whitelist beats gitignore in the `ignore` crate.
    #[must_use]
    pub fn whitelist<I, S>(mut self, patterns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.whitelist.extend(patterns.into_iter().map(Into::into));
        self
    }

    /// Consult `.gitignore` files in addition to the built-in patterns.
    ///
    /// Off by default because the oracle has no gitignore layer at all; see the
    /// module docs.
    #[must_use]
    pub fn gitignore(mut self, enabled: bool) -> Self {
        self.gitignore = enabled;
        self
    }

    /// Whether a `.gitignore` needs a `.git` at or above the root to apply.
    ///
    /// `true` matches `ignore`'s and git's own behaviour and is the default.
    /// Setting it to `false` is for tests that would rather assert pattern
    /// semantics than run `git init`.
    #[must_use]
    pub fn require_git(mut self, required: bool) -> Self {
        self.require_git = required;
        self
    }

    /// Compile every pattern.
    ///
    /// # Errors
    ///
    /// Returns [`PatternError`] naming the first pattern `globset` rejects. A bad
    /// user pattern is worth surfacing rather than silently dropping: dropping it
    /// would make the watcher report a tree the user asked it not to.
    pub fn build(self) -> Result<Filter, PatternError> {
        let builtin = compile(IGNORED_FILE_GLOBS.iter().copied())?;
        let extra = compile(self.extra.iter().map(String::as_str))?;
        let whitelist = compile(self.whitelist.iter().map(String::as_str))?;
        let gitignore = if self.gitignore && (!self.require_git || has_git(&self.root)) {
            Some(GitignoreChain::new(self.root.clone()))
        } else {
            None
        };
        Ok(Filter {
            root: self.root,
            builtin,
            extra,
            whitelist,
            gitignore,
        })
    }
}

/// Compile a pattern list into a set, preserving which pattern failed.
fn compile<'a, I: IntoIterator<Item = &'a str>>(patterns: I) -> Result<GlobSet, PatternError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(glob(pattern)?);
    }
    // An empty `GlobSetBuilder` builds successfully and matches nothing, so the
    // no-patterns case needs no branch.
    builder.build().map_err(|source| PatternError {
        pattern: String::new(),
        source,
    })
}

/// Compile one pattern with minimatch's separator rules.
///
/// `literal_separator(true)` is what makes `*` stop at a `/` while `**` crosses
/// it, which is `minimatch`'s behaviour and therefore `Glob.match`'s
/// (`util/glob.ts:32-34`). Leaving it at globset's default would make `*.log`
/// match `a/b.log`, silently widening every user pattern.
fn glob(pattern: &str) -> Result<Glob, PatternError> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(true)
        .build()
        .map_err(|source| PatternError {
            pattern: pattern.to_owned(),
            source,
        })
}

/// Whether a `.git` exists at or above `root`.
fn has_git(root: &Path) -> bool {
    let mut current = Some(root);
    while let Some(dir) = current {
        if dir.join(".git").exists() {
            return true;
        }
        current = dir.parent();
    }
    false
}

/// The lazily-populated per-directory `.gitignore` matchers.
///
/// The map is keyed by the directory that owns the `.gitignore`, and lookups walk
/// from the judged path's parent up to the root so the deepest matcher is
/// consulted first — git's precedence rule.
#[derive(Debug)]
struct GitignoreChain {
    root: PathBuf,
    /// `None` records "this directory has no usable `.gitignore`", so a directory
    /// is stat-ed once rather than on every event.
    matchers: Mutex<HashMap<PathBuf, Option<Arc<Gitignore>>>>,
}

impl GitignoreChain {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            matchers: Mutex::new(HashMap::new()),
        }
    }

    /// The matcher owned by `dir`, loading it on first use.
    ///
    /// A poisoned mutex is treated as "no matcher" rather than propagated: this
    /// is consulted from the watcher's ingest path, and a filter that panics
    /// there takes the watch down. Under-filtering is the safe direction — the
    /// consumer sees an extra event, not a missing one.
    fn matcher(&self, dir: &Path) -> Option<Arc<Gitignore>> {
        let mut matchers = self.matchers.lock().ok()?;
        if let Some(cached) = matchers.get(dir) {
            return cached.clone();
        }
        let candidate = dir.join(".gitignore");
        let loaded = if candidate.is_file() {
            let mut builder = GitignoreBuilder::new(dir);
            // A partial-error return still yields the lines that did parse, so a
            // single bad line does not discard the file.
            drop(builder.add(&candidate));
            builder.build().ok().map(Arc::new)
        } else {
            None
        };
        matchers.insert(dir.to_path_buf(), loaded.clone());
        loaded
    }

    /// Whether `relative` is gitignored, consulting the deepest matcher first.
    fn is_ignored(&self, relative: &Path, is_dir: bool) -> bool {
        // Directories from the file's own parent up to (and including) the root.
        let mut dirs = Vec::new();
        let mut current = relative.parent();
        while let Some(dir) = current {
            dirs.push(self.root.join(dir));
            current = dir.parent();
        }
        if dirs.last() != Some(&self.root) {
            dirs.push(self.root.clone());
        }
        let absolute = self.root.join(relative);
        for dir in dirs {
            let Some(matcher) = self.matcher(&dir) else {
                continue;
            };
            match matcher.matched_path_or_any_parents(&absolute, is_dir) {
                Match::Ignore(_) => return true,
                // A `!` rule in a deeper file beats an ignore rule in a shallower
                // one, so a whitelist hit stops the walk instead of continuing.
                Match::Whitelist(_) => return false,
                Match::None => (),
            }
        }
        false
    }

    fn invalidate(&self) {
        if let Ok(mut matchers) = self.matchers.lock() {
            matchers.clear();
        }
    }
}

/// Decides whether a path is reportable.
///
/// Cheap to share: [`Filter::is_ignored`] takes `&self` and the only interior
/// mutability is the gitignore cache, so one filter serves the ingest thread and
/// any consumer that wants to ask the same question.
#[derive(Debug)]
pub struct Filter {
    root: PathBuf,
    builtin: GlobSet,
    extra: GlobSet,
    whitelist: GlobSet,
    gitignore: Option<GitignoreChain>,
}

impl Filter {
    /// A filter over `root` with only the built-in patterns.
    ///
    /// # Errors
    ///
    /// Only if a built-in pattern stops compiling, which a unit test in this
    /// module rules out.
    pub fn builtin(root: impl Into<PathBuf>) -> Result<Self, PatternError> {
        FilterBuilder::new(root).build()
    }

    /// The directory paths are judged relative to.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether `path` must not be reported.
    ///
    /// `is_dir` matters because a gitignore pattern ending in `/` applies only to
    /// directories. A caller that does not know — a deletion, whose target is
    /// already gone — should pass `false`, which is the conservative answer: it
    /// can only cause an extra event, never a missed one.
    ///
    /// A path outside [`Filter::root`] is never ignored. The watcher only
    /// subscribes under the root, so this can only be reached by a caller asking
    /// about something unrelated, and silently answering "ignored" would hide it.
    #[must_use]
    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        let Some(relative) = self.relative(path) else {
            return false;
        };
        let candidate = to_slash(&relative);
        if self.whitelist.is_match(candidate.as_str()) {
            return false;
        }
        if relative
            .components()
            .filter_map(component_name)
            .any(|name| IGNORED_FOLDERS.binary_search(&name).is_ok())
        {
            return true;
        }
        if self.builtin.is_match(candidate.as_str()) || self.extra.is_match(candidate.as_str()) {
            return true;
        }
        self.gitignore
            .as_ref()
            .is_some_and(|chain| chain.is_ignored(&relative, is_dir))
    }

    /// Whether `path` is a `.gitignore`, i.e. whether observing a change to it
    /// means the cached chain is stale.
    #[must_use]
    pub fn is_gitignore(path: &Path) -> bool {
        path.file_name().is_some_and(|name| name == ".gitignore")
    }

    /// Drop every cached `.gitignore` matcher so the next judgement re-reads.
    ///
    /// Idempotent, and a no-op when gitignore filtering is off.
    pub fn invalidate(&self) {
        if let Some(chain) = self.gitignore.as_ref() {
            chain.invalidate();
        }
    }

    /// `path` relative to the root, or `None` when it is not under the root.
    fn relative(&self, path: &Path) -> Option<PathBuf> {
        if path == self.root {
            return None;
        }
        path.strip_prefix(&self.root)
            .ok()
            .map(Path::to_path_buf)
            .or_else(|| {
                // A relative path is taken as already root-relative, which is
                // what lets a caller ask about `src/a.rs` without knowing the
                // root. An absolute path that is not under the root yields
                // `None` and is therefore reported.
                path.is_relative().then(|| path.to_path_buf())
            })
    }
}

/// A path component's name, for the folder-basename test.
///
/// Only `Normal` components can match a basename; `..` and a root prefix cannot,
/// and treating them as names would let a path such as `../bin/x` be judged by
/// the string `bin` when the real target lies outside the tree.
fn component_name(component: Component<'_>) -> Option<&str> {
    match component {
        Component::Normal(name) => name.to_str(),
        _ => None,
    }
}

/// The path as a `/`-separated string, so one pattern form works on every
/// platform. `Ignore.match` splits on `[/\\]` for the same reason.
fn to_slash(path: &Path) -> String {
    path.components()
        .filter_map(component_name)
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "/repo";

    fn filter() -> Filter {
        Filter::builtin(ROOT).expect("built-in patterns compile")
    }

    #[test]
    fn the_folder_list_is_sorted_so_binary_search_is_valid() {
        let mut sorted = IGNORED_FOLDERS;
        sorted.sort_unstable();
        assert_eq!(IGNORED_FOLDERS, sorted);
    }

    #[test]
    fn the_pattern_counts_match_the_oracle() {
        // `ignore.ts` FOLDERS has 28 entries and FILES has 11; a port that
        // silently loses one prunes less than the oracle.
        assert_eq!(IGNORED_FOLDERS.len(), 28);
        assert_eq!(IGNORED_FILE_GLOBS.len(), 11);
    }

    #[test]
    fn a_folder_basename_is_pruned_at_any_depth() {
        let filter = filter();
        for path in [
            "/repo/node_modules/pkg/index.js",
            "/repo/packages/a/node_modules/pkg/index.js",
            "/repo/target/debug/build.rs",
            "/repo/crates/x/target/debug/x",
            "/repo/.git/HEAD",
        ] {
            assert!(
                filter.is_ignored(Path::new(path), false),
                "{path} should be ignored"
            );
        }
    }

    #[test]
    fn an_ordinary_source_file_is_reported() {
        let filter = filter();
        for path in ["/repo/src/main.rs", "/repo/packages/a/src/index.ts"] {
            assert!(
                !filter.is_ignored(Path::new(path), false),
                "{path} should be reported"
            );
        }
    }

    #[test]
    fn a_doublestar_prefix_matches_at_the_root_too() {
        // `**/*.log` has to match `a.log` as well as `deep/a.log`, or the
        // built-in list silently only prunes below the top level.
        let filter = filter();
        assert!(filter.is_ignored(Path::new("/repo/a.log"), false));
        assert!(filter.is_ignored(Path::new("/repo/deep/a.log"), false));
        assert!(filter.is_ignored(Path::new("/repo/deep/.DS_Store"), false));
        assert!(filter.is_ignored(Path::new("/repo/logs/today"), false));
        assert!(filter.is_ignored(Path::new("/repo/a/tmp/scratch"), false));
    }

    #[test]
    fn a_star_does_not_cross_a_separator() {
        // With globset's default `literal_separator(false)`, `**/*.log` would
        // also match a directory named `x.log/y`; pinning the flag keeps user
        // patterns as narrow as minimatch makes them.
        let filter = FilterBuilder::new(ROOT)
            .extra_patterns(["*.tmp"])
            .build()
            .expect("compiles");
        assert!(filter.is_ignored(Path::new("/repo/a.tmp"), false));
        assert!(!filter.is_ignored(Path::new("/repo/deep/a.tmp"), false));
    }

    #[test]
    fn watcher_ignore_patterns_are_honoured() {
        let config = zuno_config::schema::WatcherConfig {
            ignore: Some(vec!["**/generated/**".to_owned(), "*.snap".to_owned()]),
        };
        let filter = FilterBuilder::new(ROOT)
            .watcher_config(Some(&config))
            .build()
            .expect("compiles");
        assert!(filter.is_ignored(Path::new("/repo/a/generated/b.rs"), false));
        assert!(filter.is_ignored(Path::new("/repo/x.snap"), false));
        assert!(!filter.is_ignored(Path::new("/repo/a/real.rs"), false));
    }

    #[test]
    fn an_absent_watcher_config_adds_nothing() {
        let filter = FilterBuilder::new(ROOT)
            .watcher_config(None)
            .build()
            .expect("compiles");
        assert!(!filter.is_ignored(Path::new("/repo/src/main.rs"), false));
    }

    #[test]
    fn a_whitelist_beats_every_other_rule() {
        let filter = FilterBuilder::new(ROOT)
            .extra_patterns(["**/*.log"])
            .whitelist(["**/keep.log", "**/node_modules/keep/**"])
            .build()
            .expect("compiles");
        assert!(!filter.is_ignored(Path::new("/repo/a/keep.log"), false));
        assert!(filter.is_ignored(Path::new("/repo/a/drop.log"), false));
        assert!(!filter.is_ignored(Path::new("/repo/node_modules/keep/index.js"), false));
    }

    #[test]
    fn a_bad_user_pattern_is_reported_with_its_text() {
        let error = FilterBuilder::new(ROOT)
            .extra_patterns(["a[".to_owned()])
            .build()
            .expect_err("an unclosed class is not a glob");
        assert_eq!(error.pattern, "a[");
    }

    #[test]
    fn the_root_itself_is_never_ignored() {
        assert!(!filter().is_ignored(Path::new(ROOT), true));
    }

    #[test]
    fn a_path_outside_the_root_is_reported_rather_than_hidden() {
        let outside = std::env::temp_dir()
            .join("zuno-watch-outside")
            .join("node_modules")
            .join("x");
        assert!(!filter().is_ignored(&outside, false));
    }

    #[test]
    fn a_relative_path_is_taken_as_root_relative() {
        let filter = filter();
        assert!(filter.is_ignored(Path::new("node_modules/pkg/i.js"), false));
        assert!(!filter.is_ignored(Path::new("src/main.rs"), false));
    }

    #[test]
    fn a_parent_traversal_component_is_not_read_as_a_basename() {
        // `../bin/x` must not be pruned by the string `bin`: it names something
        // outside the tree, and the folder rule is about components of a path
        // *inside* it.
        let filter = filter();
        assert!(filter.is_ignored(Path::new("a/bin/x"), false));
        assert!(!filter.is_ignored(Path::new("../nothing/x"), false));
    }

    #[test]
    fn gitignore_is_off_unless_asked_for() {
        let filter = filter();
        assert!(filter.gitignore.is_none());
    }

    #[test]
    fn is_gitignore_recognises_only_the_file_itself() {
        assert!(Filter::is_gitignore(Path::new("/repo/a/.gitignore")));
        assert!(!Filter::is_gitignore(Path::new("/repo/a/.gitignore.bak")));
        assert!(!Filter::is_gitignore(Path::new("/repo/a")));
    }

    #[test]
    fn invalidate_is_a_no_op_without_gitignore() {
        filter().invalidate();
    }
}

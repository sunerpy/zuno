//! Skill discovery roots, in Zuno scope order.
//!
//! Zuno owns this composition. Project-local Zuno and Agent Skills are surfaced
//! before user-global roots, matching Codex's repo-before-user scope. OpenCode
//! directories are deliberately absent: a Zuno process never scans another
//! product's private configuration tree.
//!
//! | order | root | pattern | `dot` |
//! |---|------|---------|-------|
//! | 1 | every project `.zuno` from `directory` up to `worktree` | `{skill,skills}/**/SKILL.md` | no |
//! | 2 | every project `.agents` | `skills/**/SKILL.md` | yes |
//! | 3 | Zuno's global/configured config directories | `{skill,skills}/**/SKILL.md` | no |
//! | 4 | `$HOME/.agents` | `skills/**/SKILL.md` | yes |
//! | 5 | each `skills.paths[]` entry | `**/SKILL.md` | no |
//! | 6 | each cache dir a `skills.urls[]` index produced | `**/SKILL.md` | no |
//!
//! Order is presentation and provenance, not a hidden same-name winner. Distinct
//! sources with the same declared name remain independently selectable.
//!
//! # Rules that are not obvious from the table
//!
//! **`ZUNO_DISABLE_EXTERNAL_SKILLS` leaves Zuno roots alone.** It removes only
//! `.agents`; project/global `.zuno` and explicit paths remain.
//!
//! **`ZUNO_DISABLE_PROJECT_CONFIG` gates project `.zuno`, not Agent Skills.** A
//! project `.agents` skill remains available because it is a capability scope,
//! not a Zuno configuration layer.
//!
//! **`skills.paths[]` relative entries resolve against `directory`, not the
//! worktree.** A process started in a repository subdirectory therefore resolves
//! `relskills` below that subdirectory, not below the repository root.
//!
//! **Path identity is canonical when the filesystem can resolve it.** A symlinked
//! package and its target are one source, while the first discovery spelling is
//! retained for display. This follows Codex's canonical de-duplication without
//! losing the project/global provenance that found the package first.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use zuno_config::Config;
use zuno_config::schema::{SkillCatalogExposure, SkillPathConfig};
use zuno_paths::{Env, Layout, PROJECT_CONFIG_DIRECTORY, node_path, walk};

use crate::skill::scan::{self, EXTERNAL_PREFIXES, ROOT_PREFIXES, ZUNO_PREFIXES};
use crate::skill::{SkillWarning, SkillWarningKind};

/// Standard shared Agent Skills directory.
pub const AGENTS_EXTERNAL_DIR: &str = ".agents";

/// Removes external Agent Skills roots while retaining Zuno-native roots.
pub const ZUNO_DISABLE_EXTERNAL_SKILLS: &str = "ZUNO_DISABLE_EXTERNAL_SKILLS";

/// Everything skill discovery needs, with no hidden process state.
///
/// Production callers use [`SkillOptions::from_process`]. Tests use
/// [`SkillOptions::new`] with an explicit [`Env`] and [`SkillOptions::with_layout`],
/// because mutating the process environment is `unsafe` and this workspace forbids
/// it.
#[derive(Debug, Clone)]
pub struct SkillOptions {
    directory: PathBuf,
    worktree: Option<PathBuf>,
    env: Env,
    layout: Layout,
    paths: Vec<String>,
    urls: Vec<String>,
    raw_path_config: Vec<SkillPathConfig>,
    path_config: Vec<ResolvedSkillPathConfig>,
    external_disabled: bool,
}

impl SkillOptions {
    /// Build from an explicit environment snapshot and an already-merged
    /// `skills.paths` / `skills.urls`.
    #[must_use]
    pub fn new(
        directory: impl Into<PathBuf>,
        worktree: Option<impl Into<PathBuf>>,
        env: &Env,
        paths: Vec<String>,
        urls: Vec<String>,
    ) -> Self {
        Self {
            directory: directory.into(),
            worktree: worktree.map(Into::into),
            env: env.clone(),
            layout: Layout::resolve(env),
            paths,
            urls,
            raw_path_config: Vec::new(),
            path_config: Vec::new(),
            external_disabled: env.flag(ZUNO_DISABLE_EXTERNAL_SKILLS),
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
        let (paths, urls, path_config) = config.skills.as_ref().map_or_else(
            || (Vec::new(), Vec::new(), Vec::new()),
            |skills| {
                (
                    skills.paths.clone().unwrap_or_default(),
                    skills.urls.clone().unwrap_or_default(),
                    skills.config.clone().unwrap_or_default(),
                )
            },
        );
        Self::new(directory, worktree, env, paths, urls).with_path_config(path_config)
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

    /// Override the resolved layout, for a test that needs a fabricated home or
    /// config directory without touching the environment.
    #[must_use]
    pub fn with_layout(mut self, layout: Layout) -> Self {
        self.layout = layout;
        self.path_config = self
            .raw_path_config
            .iter()
            .cloned()
            .map(|entry| ResolvedSkillPathConfig::new(entry, &self))
            .collect();
        self
    }

    /// Add ordered per-path enablement and catalog-exposure rules.
    ///
    /// Paths are expanded after the layout is known. Matching is canonical when
    /// the target exists and lexical otherwise, so a configured path may safely
    /// refer to a Skill that will be installed later.
    #[must_use]
    pub fn with_path_config(mut self, entries: Vec<SkillPathConfig>) -> Self {
        self.path_config = entries
            .iter()
            .cloned()
            .map(|entry| ResolvedSkillPathConfig::new(entry, &self))
            .collect();
        self.raw_path_config = entries;
        self
    }

    /// The directory the session is anchored at, and what `skills.paths[]`
    /// relative entries resolve against.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Where the ancestor walk stops, when there is a worktree.
    #[must_use]
    pub fn worktree(&self) -> Option<&Path> {
        self.worktree.as_deref()
    }

    /// The resolved path layout.
    #[must_use]
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// The environment snapshot that determined discovery and watcher policy.
    #[must_use]
    pub fn env(&self) -> &Env {
        &self.env
    }

    /// Whether implicit shared Agent Skills roots are suppressed.
    #[must_use]
    pub fn external_disabled(&self) -> bool {
        self.external_disabled
    }

    /// The configured `skills.urls[]`, in config order.
    #[must_use]
    pub fn urls(&self) -> &[String] {
        &self.urls
    }

    /// Existing roots, or their nearest existing parent, that can change discovery.
    ///
    /// The list is de-duplicated and removes nested roots when an existing parent is
    /// already watched recursively. It never invents a source outside the standard,
    /// configured, or remote-cache scopes used by [`SkillSources::discover`].
    #[must_use]
    pub fn watch_roots(&self) -> Vec<PathBuf> {
        let mut candidates = vec![
            self.worktree
                .clone()
                .unwrap_or_else(|| self.directory.clone()),
            self.layout.config().to_path_buf(),
            self.layout.home().join(".zuno"),
            self.layout.home().join(AGENTS_EXTERNAL_DIR),
            self.remote_cache_root(),
        ];
        candidates.extend(
            self.layout
                .config_directories(&self.directory, self.worktree.as_deref()),
        );
        candidates.extend(self.paths.iter().map(|entry| expand_path(entry, self)));
        candidates.extend(
            self.raw_path_config
                .iter()
                .map(|entry| expand_path(&entry.path, self)),
        );

        let mut roots = candidates
            .into_iter()
            .filter_map(nearest_existing_watch_root)
            .collect::<Vec<_>>();
        roots.sort_by_key(|path| path.components().count());
        let mut deduplicated = Vec::<PathBuf>::new();
        for root in roots {
            if deduplicated.iter().any(|parent| root.starts_with(parent)) {
                continue;
            }
            deduplicated.push(root);
        }
        deduplicated
    }

    /// Where a pulled remote skill is cached: `$XDG_CACHE_HOME/zuno/skills`.
    #[must_use]
    pub fn remote_cache_root(&self) -> PathBuf {
        self.layout.cache().join("skills")
    }

    /// Standard Agent Skills roots used by both the home probe and project walk.
    fn external_dirs(&self) -> Vec<&'static str> {
        if self.external_disabled {
            return Vec::new();
        }
        vec![AGENTS_EXTERNAL_DIR]
    }
}

#[derive(Debug, Clone)]
struct ResolvedSkillPathConfig {
    target_key: String,
    target_is_skill_file: bool,
    enabled: bool,
    exposure: Option<SkillCatalogExposure>,
    recursive: bool,
}

impl ResolvedSkillPathConfig {
    fn new(entry: SkillPathConfig, options: &SkillOptions) -> Self {
        let expanded = expand_path(&entry.path, options);
        let target_is_skill_file = expanded.is_file()
            || expanded
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("SKILL.md"));
        let target = canonical_or_normalized(&expanded);
        Self {
            target_key: path_key(&target),
            target_is_skill_file,
            enabled: entry.enabled.unwrap_or(true),
            exposure: entry.exposure,
            recursive: entry.recursive.unwrap_or(false),
        }
    }

    fn matches(&self, skill_path: &Path) -> bool {
        let skill = canonical_or_normalized(skill_path);
        let skill_key = path_key(&skill);
        if self.target_is_skill_file {
            return skill_key == self.target_key;
        }
        if self.recursive {
            return key_is_within(&skill_key, &self.target_key);
        }
        skill
            .parent()
            .is_some_and(|parent| path_key(parent) == self.target_key)
    }
}

#[derive(Debug, Clone, Copy)]
struct EffectivePathConfig {
    enabled: bool,
    exposure: Option<SkillCatalogExposure>,
}

fn nearest_existing_watch_root(mut path: PathBuf) -> Option<PathBuf> {
    loop {
        if path.is_dir() {
            path.parent()?;
            return Some(std::fs::canonicalize(&path).unwrap_or(path));
        }
        if !path.pop() {
            return None;
        }
    }
}

/// Which root produced a match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Root {
    /// A project `.zuno/{skill,skills}` root.
    ProjectZuno,
    /// A project `.agents/skills` root.
    ProjectAgents,
    /// A Zuno user/config directory.
    GlobalZuno,
    /// `$HOME/.agents/skills`.
    GlobalAgents,
    /// A `skills.paths[]` entry.
    ConfiguredPath,
    /// A cache directory a `skills.urls[]` index produced.
    Remote,
}

/// One discovered `SKILL.md`, with the root that found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPath {
    path: PathBuf,
    root: Root,
    exposure: Option<SkillCatalogExposure>,
}

impl SkillPath {
    /// The first discovery spelling of the absolute path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Which root found it.
    #[must_use]
    pub fn root(&self) -> Root {
        self.root
    }

    /// Final configured catalog exposure for this path, when overridden.
    #[must_use]
    pub fn exposure(&self) -> Option<SkillCatalogExposure> {
        self.exposure
    }
}

/// The local half of discovery: matches in Zuno precedence order, the directories
/// holding them, and the URLs still to pull.
#[derive(Debug, Default)]
pub struct SkillSources {
    matches: Vec<SkillPath>,
    seen: HashSet<String>,
    urls: Vec<String>,
    path_config: Vec<ResolvedSkillPathConfig>,
    disabled_sources: Vec<String>,
    warnings: Vec<SkillWarning>,
}

impl SkillSources {
    /// Local roots. Touches the filesystem but never the network; the remote root needs
    /// [`crate::skill::remote::pull`] and is applied by
    /// [`SkillSources::extend_remote`].
    #[must_use]
    pub fn discover(options: &SkillOptions) -> Self {
        let mut sources = Self {
            urls: options.urls.clone(),
            path_config: options.path_config.clone(),
            ..Self::default()
        };
        let external = options.external_dirs();

        // Project scope first. Keep the project Zuno roots so the broader config
        // directory list below can skip rescanning them.
        let project_zuno = if options.layout.project_config_disabled() {
            Vec::new()
        } else {
            walk::up(
                &[PROJECT_CONFIG_DIRECTORY],
                &options.directory,
                options.worktree.as_deref(),
            )
        };
        let project_zuno_identities = project_zuno
            .iter()
            .map(|root| identity(root))
            .collect::<HashSet<_>>();
        for root in project_zuno {
            sources.absorb(&root, ZUNO_PREFIXES, false, Root::ProjectZuno);
        }

        if !external.is_empty() {
            for root in walk::up(&external, &options.directory, options.worktree.as_deref()) {
                sources.absorb(&root, EXTERNAL_PREFIXES, true, Root::ProjectAgents);
            }
        }

        // Global Zuno roots before global external-product roots. The shared
        // config-chain helper also returns project `.zuno` directories; those
        // were already scanned above with project provenance.
        for dir in options
            .layout
            .config_directories(&options.directory, options.worktree.as_deref())
        {
            if project_zuno_identities.contains(&identity(&dir)) {
                continue;
            }
            sources.absorb(&dir, ZUNO_PREFIXES, false, Root::GlobalZuno);
        }

        for dir in &external {
            let root = options.layout.home().join(dir);
            if !root.is_dir() {
                continue;
            }
            sources.absorb(&root, EXTERNAL_PREFIXES, true, Root::GlobalAgents);
        }

        // Explicit paths follow native project/user scopes.
        for entry in &options.paths {
            let dir = expand_path(entry, options);
            if !dir.is_dir() {
                sources.warnings.push(SkillWarning::new(
                    dir.to_string_lossy().as_ref(),
                    SkillWarningKind::PathNotFound,
                ));
                continue;
            }
            sources.absorb(&dir, ROOT_PREFIXES, false, Root::ConfiguredPath);
        }

        sources
    }

    /// Fold in the cache directories a `skills.urls[]` index produced.
    pub fn extend_remote(&mut self, dirs: &[PathBuf]) {
        for dir in dirs {
            self.absorb(dir, ROOT_PREFIXES, false, Root::Remote);
        }
    }

    fn absorb(&mut self, root: &Path, prefixes: &[&str], dot: bool, provenance: Root) {
        let found = scan::scan(root, prefixes, dot);
        for (at, kind) in found.errors {
            self.warnings.push(SkillWarning::new(
                at.to_string_lossy().as_ref(),
                SkillWarningKind::ScanFailed(kind),
            ));
        }
        for path in found.matches {
            let key = identity(&path);
            if !self.seen.insert(key) {
                continue;
            }
            let policy = self.path_policy(&path);
            if policy.is_some_and(|policy| !policy.enabled) {
                self.disabled_sources
                    .push(path.to_string_lossy().into_owned());
                continue;
            }
            self.matches.push(SkillPath {
                path,
                root: provenance,
                exposure: policy.and_then(|policy| policy.exposure),
            });
        }
    }

    fn path_policy(&self, path: &Path) -> Option<EffectivePathConfig> {
        self.path_config
            .iter()
            .filter(|entry| entry.matches(path))
            .map(|entry| EffectivePathConfig {
                enabled: entry.enabled,
                exposure: entry.exposure,
            })
            .next_back()
    }

    /// Every match, in discovery order, de-duplicated by path.
    #[must_use]
    pub fn matches(&self) -> &[SkillPath] {
        &self.matches
    }

    /// The directory of every discovered match, preserving discovery order.
    #[must_use]
    pub fn dirs(&self) -> Vec<PathBuf> {
        let mut seen = HashSet::new();
        let mut dirs = Vec::new();
        for entry in &self.matches {
            if let Some(parent) = entry.path.parent()
                && seen.insert(parent.to_path_buf())
            {
                dirs.push(parent.to_path_buf());
            }
        }
        dirs
    }

    /// The `skills.urls[]` still to pull, in config order.
    #[must_use]
    pub fn urls(&self) -> &[String] {
        &self.urls
    }

    /// Discovery-time warnings: unusable `skills.paths[]` entries and roots that
    /// could not be traversed.
    #[must_use]
    pub fn warnings(&self) -> &[SkillWarning] {
        &self.warnings
    }

    pub(crate) fn take_disabled_sources(&mut self) -> Vec<String> {
        std::mem::take(&mut self.disabled_sources)
    }

    pub(crate) fn take_warnings(&mut self) -> Vec<SkillWarning> {
        std::mem::take(&mut self.warnings)
    }

    pub(crate) fn push_warning(&mut self, warning: SkillWarning) {
        self.warnings.push(warning);
    }
}

/// `skills.paths[]` expansion: `~/` against `$HOME`, then anything still
/// relative against `directory`.
fn expand_path(entry: &str, options: &SkillOptions) -> PathBuf {
    let expanded = match entry.strip_prefix("~/") {
        Some(rest) => options.layout.home().join(rest),
        None => PathBuf::from(entry),
    };
    if expanded.is_absolute() {
        expanded
    } else {
        options.directory.join(expanded)
    }
}

/// Canonical source identity, with a normalized path fallback for races and
/// unreadable entries. The first discovery path remains the advertised locator.
fn identity(path: &Path) -> String {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    node_path::normalize(&resolved.to_string_lossy())
}

fn canonical_or_normalized(path: &Path) -> PathBuf {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| PathBuf::from(node_path::normalize(&path.to_string_lossy())))
}

fn path_key(path: &Path) -> String {
    let normalized = node_path::normalize(&path.to_string_lossy());
    #[cfg(windows)]
    {
        normalized.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        normalized
    }
}

fn key_is_within(candidate: &str, root: &str) -> bool {
    candidate == root
        || candidate.strip_prefix(root).is_some_and(|suffix| {
            suffix.starts_with(std::path::MAIN_SEPARATOR)
                || suffix.starts_with('/')
                || suffix.starts_with('\\')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;
    use zuno_paths::env::{HOME, XDG_CACHE_HOME, XDG_CONFIG_HOME, ZUNO_CONFIG_DIR};

    struct Fixture {
        dir: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                dir: TempDir::new().expect("tempdir"),
            }
        }

        fn home(&self) -> PathBuf {
            self.dir.path().join("home")
        }

        fn skill(&self, relative: &str, name: &str) -> PathBuf {
            let path = self.dir.path().join(relative).join("SKILL.md");
            fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            fs::write(
                &path,
                format!("---\nname: {name}\ndescription: d\n---\nB\n"),
            )
            .expect("write");
            path
        }

        fn env(&self) -> Env {
            let home = self.home();
            Env::empty()
                .with(HOME, home.to_string_lossy().as_ref())
                .with(
                    XDG_CONFIG_HOME,
                    home.join(".config").to_string_lossy().as_ref(),
                )
                .with(
                    XDG_CACHE_HOME,
                    home.join(".cache").to_string_lossy().as_ref(),
                )
        }

        fn options(&self, cwd: &str, paths: Vec<String>) -> SkillOptions {
            SkillOptions::new(
                self.dir.path().join(cwd),
                None::<PathBuf>,
                &self.env(),
                paths,
                Vec::new(),
            )
        }
    }

    fn roots(sources: &SkillSources) -> Vec<(Root, PathBuf)> {
        sources
            .matches()
            .iter()
            .map(|entry| (entry.root(), entry.path().to_path_buf()))
            .collect()
    }

    #[test]
    fn every_local_root_is_visited_in_zuno_scope_order() {
        let fixture = Fixture::new();
        let project_zuno = fixture.skill("proj/sub/.zuno/skills/pz", "pz");
        let project_agents = fixture.skill("proj/sub/.agents/skills/pa", "pa");
        fixture.skill("proj/.claude/skills/pc", "ignored-project-claude");
        let config = fixture.skill("home/.config/zuno/skill/gz", "gz");
        let agents = fixture.skill("home/.agents/skills/ga", "ga");
        fixture.skill("home/.claude/skills/gc", "ignored-global-claude");
        let configured = fixture.skill("extra/e", "e");
        fs::create_dir_all(fixture.dir.path().join("proj/sub")).expect("mkdir");

        let options = fixture.options(
            "proj/sub",
            vec![
                fixture
                    .dir
                    .path()
                    .join("extra")
                    .to_string_lossy()
                    .into_owned(),
            ],
        );
        let sources = SkillSources::discover(&options);

        assert_eq!(
            roots(&sources),
            vec![
                (Root::ProjectZuno, project_zuno),
                (Root::ProjectAgents, project_agents),
                (Root::GlobalZuno, config),
                (Root::GlobalAgents, agents),
                (Root::ConfiguredPath, configured),
            ]
        );
        assert!(sources.warnings().is_empty(), "{:?}", sources.warnings());
    }

    #[test]
    fn opencode_directories_are_not_skill_sources() {
        let fixture = Fixture::new();
        fixture.skill("home/.config/opencode/skill/wrong", "wrong");
        fixture.skill("xdg/opencode/skills/still-wrong", "still-wrong");
        fixture.skill("proj/.opencode/skills/project-wrong", "project-wrong");
        let env = fixture.env().with(
            XDG_CONFIG_HOME,
            fixture.dir.path().join("xdg").to_string_lossy().as_ref(),
        );
        let options = SkillOptions::new(
            fixture.dir.path().join("proj"),
            None::<PathBuf>,
            &env,
            Vec::new(),
            Vec::new(),
        );

        assert!(roots(&SkillSources::discover(&options)).is_empty());
    }

    #[test]
    fn claude_directories_are_never_implicit_skill_sources() {
        let fixture = Fixture::new();
        fixture.skill("home/.claude/skills/a", "a");
        fixture.skill("proj/.claude/skills/c", "c");
        let agents = fixture.skill("home/.agents/skills/b", "b");
        let project = fixture.skill("proj/.agents/skills/d", "d");

        let options = SkillOptions::new(
            fixture.dir.path().join("proj"),
            None::<PathBuf>,
            &fixture.env(),
            Vec::new(),
            Vec::new(),
        );
        let sources = SkillSources::discover(&options);
        assert_eq!(
            roots(&sources),
            vec![(Root::ProjectAgents, project), (Root::GlobalAgents, agents),]
        );
    }

    #[test]
    fn a_claude_directory_can_still_be_selected_as_an_explicit_path() {
        let fixture = Fixture::new();
        let selected = fixture.skill("home/.claude/skills/a", "a");
        let options = SkillOptions::new(
            fixture.dir.path().join("proj"),
            None::<PathBuf>,
            &fixture.env(),
            vec![
                fixture
                    .home()
                    .join(".claude/skills")
                    .to_string_lossy()
                    .into_owned(),
            ],
            Vec::new(),
        );

        assert_eq!(
            roots(&SkillSources::discover(&options)),
            vec![(Root::ConfiguredPath, selected)]
        );
    }

    #[test]
    fn external_switch_removes_agent_skills_but_keeps_zuno_roots() {
        let fixture = Fixture::new();
        fixture.skill("home/.config/opencode/skill/o", "o");
        fixture.skill("home/.claude/skills/a", "a");
        fixture.skill("home/.agents/skills/b", "b");
        fixture.skill("proj/.opencode/skills/po", "po");
        fixture.skill("proj/.agents/skills/c", "c");
        let project_zuno = fixture.skill("proj/.zuno/skills/pz", "pz");
        let config = fixture.skill("home/.config/zuno/skills/d", "d");

        let env = fixture.env().with("ZUNO_DISABLE_EXTERNAL_SKILLS", "1");
        let options = SkillOptions::new(
            fixture.dir.path().join("proj"),
            None::<PathBuf>,
            &env,
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(
            roots(&SkillSources::discover(&options)),
            vec![
                (Root::ProjectZuno, project_zuno),
                (Root::GlobalZuno, config),
            ]
        );
    }

    #[test]
    fn project_config_switch_only_removes_project_zuno() {
        let fixture = Fixture::new();
        fixture.skill("proj/.zuno/skills/zuno", "zuno");
        let agents = fixture.skill("proj/.agents/skills/agents", "agents");
        let env = fixture.env().with("ZUNO_DISABLE_PROJECT_CONFIG", "1");
        let options = SkillOptions::new(
            fixture.dir.path().join("proj"),
            None::<PathBuf>,
            &env,
            Vec::new(),
            Vec::new(),
        );

        assert_eq!(
            roots(&SkillSources::discover(&options)),
            vec![(Root::ProjectAgents, agents)]
        );
    }

    #[test]
    fn configured_relative_paths_resolve_against_directory_not_worktree() {
        let fixture = Fixture::new();
        let under_cwd = fixture.skill("proj/sub/extra/a", "a");
        fixture.skill("proj/extra/b", "b");

        let options = SkillOptions::new(
            fixture.dir.path().join("proj/sub"),
            Some(fixture.dir.path().join("proj")),
            &fixture.env(),
            vec!["extra".to_string()],
            Vec::new(),
        );
        assert_eq!(
            roots(&SkillSources::discover(&options)),
            vec![(Root::ConfiguredPath, under_cwd)]
        );
    }

    #[test]
    fn configured_tilde_paths_expand_against_home() {
        let fixture = Fixture::new();
        let tilde = fixture.skill("home/tilde/a", "a");
        let options = fixture.options("proj", vec!["~/tilde".to_string()]);
        assert_eq!(
            roots(&SkillSources::discover(&options)),
            vec![(Root::ConfiguredPath, tilde)]
        );
    }

    #[test]
    fn a_configured_path_that_is_not_a_directory_warns_and_continues() {
        let fixture = Fixture::new();
        let good = fixture.skill("extra/a", "a");
        let options = fixture.options(
            "proj",
            vec![
                fixture
                    .dir
                    .path()
                    .join("absent")
                    .to_string_lossy()
                    .into_owned(),
                fixture
                    .dir
                    .path()
                    .join("extra")
                    .to_string_lossy()
                    .into_owned(),
            ],
        );
        let sources = SkillSources::discover(&options);
        assert_eq!(roots(&sources), vec![(Root::ConfiguredPath, good)]);
        assert_eq!(sources.warnings().len(), 1);
        assert!(
            sources.warnings()[0]
                .to_string()
                .contains("skill path not found"),
            "{}",
            sources.warnings()[0]
        );
    }

    #[test]
    fn the_same_file_reached_by_two_roots_is_one_match() {
        let fixture = Fixture::new();
        let shared = fixture.skill("home/.agents/skills/a", "a");
        // `skills.paths[]` pointed straight at the `.agents` root reaches the
        // very same absolute path.
        let options = fixture.options(
            "proj",
            vec![
                fixture
                    .home()
                    .join(".agents/skills")
                    .to_string_lossy()
                    .into_owned(),
            ],
        );
        let sources = SkillSources::discover(&options);
        assert_eq!(roots(&sources), vec![(Root::GlobalAgents, shared)]);
    }

    #[test]
    fn project_zuno_leads_and_opencode_is_not_a_source() {
        let fixture = Fixture::new();
        fixture.skill(
            "home/.config/opencode/skill/ignored-global",
            "ignored-global",
        );
        fixture.skill("proj/.opencode/skills/ignored-project", "ignored-project");
        let project_zuno = fixture.skill("proj/sub/.zuno/skills/project-zuno", "project-zuno");
        let project_agents =
            fixture.skill("proj/sub/.agents/skills/project-agents", "project-agents");
        let global_zuno = fixture.skill("home/.config/zuno/skill/global-zuno", "global-zuno");
        let global_agents = fixture.skill("home/.agents/skills/global-agents", "global-agents");
        fs::create_dir_all(fixture.dir.path().join("proj/sub")).expect("mkdir");

        let sources = SkillSources::discover(&fixture.options("proj/sub", Vec::new()));
        let paths = sources
            .matches()
            .iter()
            .map(|entry| entry.path().to_path_buf())
            .collect::<Vec<_>>();

        assert_eq!(
            paths,
            vec![project_zuno, project_agents, global_zuno, global_agents,],
            "project Zuno and Agent Skills must precede global sources, and .opencode \
             directories must never enter the Zuno catalog"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_alias_is_one_canonical_source() {
        let fixture = Fixture::new();
        let real = fixture.skill("home/.agents/skills/a", "a");
        fs::create_dir_all(fixture.dir.path().join("aliases")).expect("mkdir");
        std::os::unix::fs::symlink(
            fixture.home().join(".agents/skills/a"),
            fixture.dir.path().join("aliases/a"),
        )
        .expect("symlink");

        let options = fixture.options(
            "proj",
            vec![
                fixture
                    .dir
                    .path()
                    .join("aliases")
                    .to_string_lossy()
                    .into_owned(),
            ],
        );
        let sources = SkillSources::discover(&options);
        assert_eq!(roots(&sources), vec![(Root::GlobalAgents, real)]);
    }

    #[test]
    fn config_dir_override_is_part_of_the_zuno_config_root() {
        let fixture = Fixture::new();
        let overridden = fixture.skill("elsewhere/skill/a", "a");
        let env = fixture.env().with(
            ZUNO_CONFIG_DIR,
            fixture
                .dir
                .path()
                .join("elsewhere")
                .to_string_lossy()
                .as_ref(),
        );
        let options = SkillOptions::new(
            fixture.dir.path().join("proj"),
            None::<PathBuf>,
            &env,
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(
            roots(&SkillSources::discover(&options)),
            vec![(Root::GlobalZuno, overridden)]
        );
    }

    #[test]
    fn dirs_are_the_parents_of_every_match_in_order() {
        let fixture = Fixture::new();
        let a = fixture.skill("home/.agents/skills/a", "a");
        let b = fixture.skill("home/.agents/skills/b", "b");
        let sources = SkillSources::discover(&fixture.options("proj", Vec::new()));
        assert_eq!(
            sources.dirs(),
            vec![
                a.parent().expect("parent").to_path_buf(),
                b.parent().expect("parent").to_path_buf()
            ]
        );
    }

    #[test]
    fn remote_cache_root_follows_xdg_cache_home() {
        let fixture = Fixture::new();
        let options = fixture.options("proj", Vec::new());
        assert_eq!(
            options.remote_cache_root(),
            fixture.home().join(".cache/zuno/skills")
        );
    }

    #[test]
    fn path_config_for_a_future_skill_contributes_its_existing_watch_ancestor() {
        let fixture = Fixture::new();
        let project = fixture.dir.path().join("project");
        let external = fixture.dir.path().join("external");
        let env = fixture.env();
        for directory in [
            &project,
            &external,
            &fixture.home(),
            &fixture.home().join(".zuno"),
            &fixture.home().join(".agents"),
            &fixture.home().join(".config/zuno"),
            &fixture.home().join(".cache/zuno/skills"),
        ] {
            fs::create_dir_all(directory).expect("watch root");
        }
        let future = external.join("future/SKILL.md");
        let options = SkillOptions::new(&project, Some(&project), &env, Vec::new(), Vec::new())
            .with_path_config(vec![SkillPathConfig {
                path: future.to_string_lossy().into_owned(),
                enabled: Some(true),
                exposure: Some(SkillCatalogExposure::Search),
                recursive: None,
            }]);

        let external = fs::canonicalize(external).expect("canonical external");
        assert!(
            options.watch_roots().contains(&external),
            "{:?}",
            options.watch_roots()
        );
    }
}

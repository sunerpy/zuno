//! The six roots, in the oracle's order.
//!
//! Port of `discoverSkills` (`packages/opencode/src/skill/index.ts:173-233`).
//! Every root below was confirmed against `opencode debug skill` 1.18.13 with a
//! fixture built for it; the citations are the oracle lines, the parenthetical
//! notes are what the fixture showed.
//!
//! | # | root | pattern | `dot` | oracle |
//! |---|------|---------|-------|--------|
//! | 1 | `$HOME/.claude` | `skills/**/SKILL.md` | yes | `:187`, `:191-193` |
//! | 2 | `$HOME/.agents` | `skills/**/SKILL.md` | yes | `:188`, `:191-193` |
//! | 3 | every `.claude`/`.agents` from `directory` up to `worktree` | `skills/**/SKILL.md` | yes | `:196-202` |
//! | 4 | every config directory | `{skill,skills}/**/SKILL.md` | no | `:205-208` |
//! | 5 | each `skills.paths[]` entry | `**/SKILL.md` | no | `:210-220` |
//! | 6 | each cache dir a `skills.urls[]` index produced | `**/SKILL.md` | no | `:222-227` |
//!
//! # Four rules that are not obvious from the table
//!
//! **The Claude switch also silences the project walk.** `externalDirs` is built
//! once (`:186-188`) and reused for both the `$HOME` probe and the ancestor walk,
//! so `OPENCODE_DISABLE_CLAUDE_CODE_SKILLS=1` removes `.claude` from *both*.
//! Confirmed: a project `.claude/skills/x/SKILL.md` disappears with the flag set.
//!
//! **`OPENCODE_DISABLE_PROJECT_CONFIG` does not reach root 3.** It gates
//! `ConfigPaths.directories` (root 4), not `fsys.up` (root 3). Confirmed: with the
//! flag set, a project `.agents` skill is still found.
//!
//! **`skills.paths[]` relative entries resolve against `directory`, not the
//! worktree** — `path.join(directory, expanded)` at `:213`. Confirmed inside a git
//! repository with the process in a subdirectory: `relskills` was found under the
//! subdirectory and *not* under the repository root. (The plan's wording, "relative
//! to workspace", does not match the oracle.)
//!
//! **The path set is keyed by the walked path string, not the resolved one.**
//! The oracle's `state.matches` is a `Set<string>` of absolute paths (`:168`) and
//! nothing canonicalizes them. A `~/.claude/skills/lark-im -> ~/.agents/skills/lark-im`
//! symlink therefore yields *two* matches with the same `name`, which is the
//! duplicate-**name** case, not the duplicate-**path** case. Keeping that
//! distinction is what makes this port report the same `location` the oracle does
//! for the user's real tree, where 27 of the 136 skills are exactly this alias.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use oc_config::Config;
use oc_paths::{Env, Layout, node_path, walk};

use crate::skill::scan::{self, EXTERNAL_PREFIXES, OPENCODE_PREFIXES, ROOT_PREFIXES};
use crate::skill::{SkillWarning, SkillWarningKind};

/// `CLAUDE_EXTERNAL_DIR` (`skill/index.ts:21`).
pub const CLAUDE_EXTERNAL_DIR: &str = ".claude";

/// `AGENTS_EXTERNAL_DIR` (`skill/index.ts:22`).
pub const AGENTS_EXTERNAL_DIR: &str = ".agents";

/// `OPENCODE_DISABLE_EXTERNAL_SKILLS` (`effect/runtime-flags.ts:22`). Removes
/// roots 1, 2 and 3 outright.
pub const OPENCODE_DISABLE_EXTERNAL_SKILLS: &str = "OPENCODE_DISABLE_EXTERNAL_SKILLS";

/// `OPENCODE_DISABLE_CLAUDE_CODE` (`effect/runtime-flags.ts:28`) — the broad
/// switch. Either this or the targeted one drops `.claude`.
pub const OPENCODE_DISABLE_CLAUDE_CODE: &str = "OPENCODE_DISABLE_CLAUDE_CODE";

/// `OPENCODE_DISABLE_CLAUDE_CODE_SKILLS` (`effect/runtime-flags.ts:29`) — the
/// targeted switch.
pub const OPENCODE_DISABLE_CLAUDE_CODE_SKILLS: &str = "OPENCODE_DISABLE_CLAUDE_CODE_SKILLS";

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
    layout: Layout,
    paths: Vec<String>,
    urls: Vec<String>,
    external_disabled: bool,
    claude_skills_disabled: bool,
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
            layout: Layout::resolve(env),
            paths,
            urls,
            external_disabled: env.flag(OPENCODE_DISABLE_EXTERNAL_SKILLS),
            claude_skills_disabled: env.flag(OPENCODE_DISABLE_CLAUDE_CODE)
                || env.flag(OPENCODE_DISABLE_CLAUDE_CODE_SKILLS),
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
        let (paths, urls) = config.skills.as_ref().map_or_else(
            || (Vec::new(), Vec::new()),
            |skills| {
                (
                    skills.paths.clone().unwrap_or_default(),
                    skills.urls.clone().unwrap_or_default(),
                )
            },
        );
        Self::new(directory, worktree, env, paths, urls)
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

    /// Whether roots 1-3 are suppressed — `flags.disableExternalSkills`.
    #[must_use]
    pub fn external_disabled(&self) -> bool {
        self.external_disabled
    }

    /// Whether `.claude` is suppressed in roots 1 and 3 —
    /// `flags.disableClaudeCodeSkills`.
    #[must_use]
    pub fn claude_skills_disabled(&self) -> bool {
        self.claude_skills_disabled
    }

    /// The configured `skills.urls[]`, in config order.
    #[must_use]
    pub fn urls(&self) -> &[String] {
        &self.urls
    }

    /// Where a pulled remote skill is cached: `$XDG_CACHE_HOME/opencode/skills`
    /// (`discovery.ts:35`).
    #[must_use]
    pub fn remote_cache_root(&self) -> PathBuf {
        self.layout.cache().join("skills")
    }

    /// `externalDirs` (`skill/index.ts:185-188`), used by roots 1-3 alike.
    fn external_dirs(&self) -> Vec<&'static str> {
        if self.external_disabled {
            return Vec::new();
        }
        let mut dirs = Vec::new();
        if !self.claude_skills_disabled {
            dirs.push(CLAUDE_EXTERNAL_DIR);
        }
        dirs.push(AGENTS_EXTERNAL_DIR);
        dirs
    }
}

/// Which root produced a match. The oracle collapses all six into one unordered
/// `Set`; keeping the provenance costs nothing and is what lets a caller explain
/// why a skill is loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Root {
    /// `$HOME/.claude/skills`.
    GlobalClaude,
    /// `$HOME/.agents/skills`.
    GlobalAgents,
    /// A `.claude` or `.agents` found by the ancestor walk.
    Project,
    /// A `{skill,skills}` directory inside a config directory.
    ConfigDirectory,
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
}

impl SkillPath {
    /// The absolute path, as walked — not canonicalized.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Which root found it.
    #[must_use]
    pub fn root(&self) -> Root {
        self.root
    }
}

/// The local half of discovery: matches in oracle order, the directories holding
/// them, and the URLs still to pull.
#[derive(Debug, Default)]
pub struct SkillSources {
    matches: Vec<SkillPath>,
    seen: HashSet<String>,
    urls: Vec<String>,
    warnings: Vec<SkillWarning>,
}

impl SkillSources {
    /// Roots 1-5. Touches the filesystem but never the network; root 6 needs
    /// [`crate::skill::remote::pull`] and is applied by
    /// [`SkillSources::extend_remote`].
    #[must_use]
    pub fn discover(options: &SkillOptions) -> Self {
        let mut sources = Self {
            urls: options.urls.clone(),
            ..Self::default()
        };
        let external = options.external_dirs();

        // Roots 1-2: the `$HOME` probes, in `externalDirs` order. The oracle
        // checks `isDir` and skips (`:192`).
        for dir in &external {
            let root = options.layout.home().join(dir);
            if !root.is_dir() {
                continue;
            }
            let provenance = if *dir == CLAUDE_EXTERNAL_DIR {
                Root::GlobalClaude
            } else {
                Root::GlobalAgents
            };
            sources.absorb(&root, EXTERNAL_PREFIXES, true, provenance);
        }

        // Root 3: the ancestor walk. `walk::up` only returns targets that exist,
        // which is why there is no `isDir` check here either.
        if !external.is_empty() {
            for root in walk::up(&external, &options.directory, options.worktree.as_deref()) {
                sources.absorb(&root, EXTERNAL_PREFIXES, true, Root::Project);
            }
        }

        // Root 4: every config directory.
        for dir in options
            .layout
            .config_directories(&options.directory, options.worktree.as_deref())
        {
            sources.absorb(&dir, OPENCODE_PREFIXES, false, Root::ConfigDirectory);
        }

        // Root 5: `skills.paths[]`.
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

    /// Root 6: fold in the cache directories a `skills.urls[]` index produced.
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
            self.matches.push(SkillPath {
                path,
                root: provenance,
            });
        }
    }

    /// Every match, in discovery order, de-duplicated by path.
    #[must_use]
    pub fn matches(&self) -> &[SkillPath] {
        &self.matches
    }

    /// `Skill.dirs()` (`skill/index.ts:306-308`): the directory of every match.
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

    pub(crate) fn take_warnings(&mut self) -> Vec<SkillWarning> {
        std::mem::take(&mut self.warnings)
    }

    pub(crate) fn push_warning(&mut self, warning: SkillWarning) {
        self.warnings.push(warning);
    }
}

/// `skills.paths[]` expansion (`skill/index.ts:212-213`): `~/` against `$HOME`,
/// then anything still relative against `directory`.
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

/// The de-duplication key: the path as a normalized string.
///
/// Deliberately **not** `canonicalize`. See the module documentation — the oracle
/// keys on the walked string, and collapsing symlink aliases here would change
/// which `location` a duplicated name reports.
fn identity(path: &Path) -> String {
    node_path::normalize(&path.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_paths::env::{HOME, OPENCODE_CONFIG_DIR, XDG_CACHE_HOME, XDG_CONFIG_HOME};
    use std::fs;
    use tempfile::TempDir;

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
    fn six_roots_are_visited_in_oracle_order() {
        let fixture = Fixture::new();
        let claude = fixture.skill("home/.claude/skills/a", "a");
        let agents = fixture.skill("home/.agents/skills/b", "b");
        let project = fixture.skill("proj/.agents/skills/c", "c");
        let config = fixture.skill("home/.config/zuno/skill/d", "d");
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
                (Root::GlobalClaude, claude),
                (Root::GlobalAgents, agents),
                (Root::Project, project),
                (Root::ConfigDirectory, config),
                (Root::ConfiguredPath, configured),
            ]
        );
        assert!(sources.warnings().is_empty(), "{:?}", sources.warnings());
    }

    #[test]
    fn claude_switch_removes_both_the_home_probe_and_the_project_walk() {
        let fixture = Fixture::new();
        fixture.skill("home/.claude/skills/a", "a");
        fixture.skill("proj/.claude/skills/c", "c");
        let agents = fixture.skill("home/.agents/skills/b", "b");
        let project = fixture.skill("proj/.agents/skills/d", "d");

        let env = fixture.env().with("ZUNO_DISABLE_CLAUDE_CODE_SKILLS", "1");
        let options = SkillOptions::new(
            fixture.dir.path().join("proj"),
            None::<PathBuf>,
            &env,
            Vec::new(),
            Vec::new(),
        );
        let sources = SkillSources::discover(&options);
        assert_eq!(
            roots(&sources),
            vec![(Root::GlobalAgents, agents), (Root::Project, project)]
        );
    }

    #[test]
    fn broad_claude_switch_behaves_like_the_targeted_one() {
        let fixture = Fixture::new();
        fixture.skill("home/.claude/skills/a", "a");
        fixture.skill("home/.agents/skills/b", "b");
        let env = fixture.env().with(OPENCODE_DISABLE_CLAUDE_CODE, "1");
        let options = SkillOptions::new(
            fixture.dir.path().join("proj"),
            None::<PathBuf>,
            &env,
            Vec::new(),
            Vec::new(),
        );
        assert!(!SkillSources::discover(&options).matches().is_empty());
        assert!(
            SkillSources::discover(&options)
                .matches()
                .iter()
                .all(|entry| entry.root() == Root::GlobalAgents)
        );
    }

    #[test]
    fn external_switch_removes_roots_one_through_three() {
        let fixture = Fixture::new();
        fixture.skill("home/.claude/skills/a", "a");
        fixture.skill("home/.agents/skills/b", "b");
        fixture.skill("proj/.agents/skills/c", "c");
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
            vec![(Root::ConfigDirectory, config)]
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
    fn a_symlink_alias_is_two_matches_because_the_oracle_does_not_canonicalize() {
        let fixture = Fixture::new();
        let real = fixture.skill("home/.agents/skills/a", "a");
        fs::create_dir_all(fixture.home().join(".claude/skills")).expect("mkdir");
        std::os::unix::fs::symlink(
            fixture.home().join(".agents/skills/a"),
            fixture.home().join(".claude/skills/a"),
        )
        .expect("symlink");

        let options = fixture.options("proj", Vec::new());
        let sources = SkillSources::discover(&options);
        assert_eq!(
            roots(&sources),
            vec![
                (
                    Root::GlobalClaude,
                    fixture.home().join(".claude/skills/a/SKILL.md")
                ),
                (Root::GlobalAgents, real),
            ]
        );
    }

    #[test]
    fn config_dir_override_is_part_of_root_four() {
        let fixture = Fixture::new();
        let overridden = fixture.skill("elsewhere/skill/a", "a");
        let env = fixture.env().with(
            OPENCODE_CONFIG_DIR,
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
            vec![(Root::ConfigDirectory, overridden)]
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
}

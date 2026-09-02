//! End-to-end [`zuno_catalog::skill::load`] behaviour over real filesystem trees.
//!
//! The unit tests inside the crate cover each root and each frontmatter rule in
//! isolation. These cover the assembly: that every root composes in the intended
//! order, that the built-in remains first, and that the two
//! de-duplication dimensions stay distinct.

use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use zuno_catalog::skill::{
    Form, Skill, SkillExposure, SkillOptions, SkillWarningKind, Skills, builtin, load, parse_file,
};
use zuno_config::schema::{SkillCatalogExposure, SkillPathConfig};
use zuno_error::ConfigError;
use zuno_paths::Env;
use zuno_paths::env::{HOME, XDG_CACHE_HOME, XDG_CONFIG_HOME};

struct Tree {
    dir: TempDir,
}

impl Tree {
    fn new() -> Self {
        Self {
            dir: TempDir::new().expect("tempdir"),
        }
    }

    fn at(&self, relative: &str) -> PathBuf {
        relative
            .split('/')
            .fold(self.dir.path().to_path_buf(), |path, component| {
                path.join(component)
            })
    }

    fn home(&self) -> PathBuf {
        self.at("home")
    }

    fn write(&self, relative: &str, body: &str) -> PathBuf {
        let path = self.at(relative);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(&path, body).expect("write");
        path
    }

    fn skill(&self, dir: &str, name: &str, description: Option<&str>) -> PathBuf {
        let front = match description {
            Some(description) => format!("---\nname: {name}\ndescription: {description}\n---\n"),
            None => format!("---\nname: {name}\n---\n"),
        };
        self.write(
            &format!("{dir}/SKILL.md"),
            &format!("{front}\nbody of {name}\n"),
        )
    }

    fn env(&self) -> Env {
        Env::empty()
            .with(HOME, self.home().to_string_lossy().as_ref())
            .with(
                XDG_CONFIG_HOME,
                self.home().join(".config").to_string_lossy().as_ref(),
            )
            .with(
                XDG_CACHE_HOME,
                self.home().join(".cache").to_string_lossy().as_ref(),
            )
    }

    fn options(&self, cwd: &str) -> SkillOptions {
        SkillOptions::new(
            self.at(cwd),
            None::<PathBuf>,
            &self.env(),
            Vec::new(),
            Vec::new(),
        )
    }

    fn options_with_paths(&self, cwd: &str, paths: Vec<String>) -> SkillOptions {
        SkillOptions::new(
            self.at(cwd),
            None::<PathBuf>,
            &self.env(),
            paths,
            Vec::new(),
        )
    }

    fn sidecar(&self, skill_dir: &str, name: &str, body: &str) -> PathBuf {
        self.write(&format!("{skill_dir}/agents/{name}.yaml"), body)
    }
}

fn names(skills: &Skills) -> Vec<String> {
    skills.all().iter().map(|s| s.name.clone()).collect()
}

fn expected_names(tail: &[&str]) -> Vec<String> {
    builtin::names()
        .map(str::to_owned)
        .chain(tail.iter().map(|name| (*name).to_owned()))
        .collect()
}

#[tokio::test]
async fn canonical_roots_contribute_and_legacy_zuno_roots_are_ignored() {
    let tree = Tree::new();
    tree.skill(
        "proj/sub/.zuno/skill/from-project-zuno",
        "from-project-zuno",
        Some("pz"),
    );
    tree.skill(
        "proj/sub/.agents/skills/from-project-agents",
        "from-project-agents",
        Some("pa"),
    );
    tree.skill(
        "proj/.claude/skills/from-project-claude",
        "from-project-claude",
        Some("pc"),
    );
    tree.skill(
        "home/.config/zuno/skill/from-config",
        "from-config",
        Some("g"),
    );
    tree.skill(
        "home/.config/zuno/skills/from-config-plural",
        "from-config-plural",
        Some("gp"),
    );
    tree.skill(
        "home/.zuno/skill/from-home-dot-zuno",
        "from-home-dot-zuno",
        Some("hz"),
    );
    tree.skill("home/.agents/skills/from-agents", "from-agents", Some("a"));
    tree.skill("home/.claude/skills/from-claude", "from-claude", Some("c"));
    tree.skill("extra/from-path", "from-path", Some("x"));
    fs::create_dir_all(tree.at("proj/sub")).expect("mkdir");

    let skills = load(&tree.options_with_paths(
        "proj/sub",
        vec![tree.at("extra").to_string_lossy().into_owned()],
    ))
    .await;

    assert_eq!(
        names(&skills),
        expected_names(&[
            "from-project-zuno",
            "from-project-agents",
            "from-config",
            "from-agents",
            "from-path",
        ])
    );
    assert!(skills.warnings().is_empty(), "{:?}", skills.warnings());
}

#[tokio::test]
async fn a_skill_file_with_no_name_is_rejected_and_the_warning_names_the_file() {
    let tree = Tree::new();
    let broken = tree.write(
        "home/.config/zuno/skill/broken/SKILL.md",
        "---\ndescription: I forgot my name.\n---\n\nBody.\n",
    );
    tree.skill("home/.config/zuno/skill/fine", "fine", Some("d"));

    let skills = load(&tree.options("proj")).await;

    assert_eq!(names(&skills), expected_names(&["fine"]));
    let rejections: Vec<&_> = skills
        .warnings()
        .iter()
        .filter(|warning| warning.kind() == &SkillWarningKind::MissingName)
        .collect();
    assert_eq!(rejections.len(), 1, "{:?}", skills.warnings());
    let message = rejections[0].to_string();
    eprintln!("QA failure scenario 1 -- rejection message: {message}");
    assert!(
        message.contains(&broken.to_string_lossy().to_string()),
        "the warning must name the file, got: {message}"
    );
    assert_eq!(rejections[0].source(), broken.to_string_lossy());
}

#[tokio::test]
async fn a_duplicate_name_keeps_both_sources_and_requires_disambiguation() {
    let tree = Tree::new();
    tree.skill("proj/.zuno/skill/dupe", "dupe", Some("from project"));
    tree.skill("home/.agents/skills/dupe", "dupe", Some("from agents"));

    let skills = load(&tree.options("proj")).await;

    assert!(skills.warnings().is_empty(), "{:?}", skills.warnings());
    assert!(skills.get("dupe").is_none());
    let sources = skills
        .named("dupe")
        .into_iter()
        .map(|skill| skill.location.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        sources,
        [
            tree.at("proj/.zuno/skill/dupe/SKILL.md").to_string_lossy(),
            tree.at("home/.agents/skills/dupe/SKILL.md")
                .to_string_lossy(),
        ]
    );
}

#[tokio::test]
async fn a_config_directory_does_not_hide_same_named_agent_skills() {
    let tree = Tree::new();
    tree.skill("home/.claude/skills/dupe", "dupe", Some("from claude"));
    tree.skill("home/.agents/skills/dupe", "dupe", Some("from agents"));
    tree.skill("home/.config/zuno/skill/dupe", "dupe", Some("from config"));

    let skills = load(&tree.options("proj")).await;

    assert_eq!(skills.named("dupe").len(), 2);
    assert!(skills.get("dupe").is_none());
    assert!(skills.warnings().is_empty());
}

/// The two de-duplication dimensions, side by side.
///
/// Left: one file, two roots -> one source. Right: two files, one name -> two
/// selectable sources. Confusing the two
/// is the defect this test exists to catch.
#[tokio::test]
async fn path_dedup_and_name_dedup_are_different_mechanisms() {
    let same_file = Tree::new();
    same_file.skill("home/.agents/skills/solo", "solo", Some("d"));
    let reached_twice = load(&same_file.options_with_paths(
        "proj",
        vec![
            same_file
                .home()
                .join(".agents/skills")
                .to_string_lossy()
                .into_owned(),
        ],
    ))
    .await;
    assert_eq!(names(&reached_twice), expected_names(&["solo"]));
    assert!(
        reached_twice.warnings().is_empty(),
        "the same path through two roots is not a duplicate name: {:?}",
        reached_twice.warnings()
    );

    let two_files = Tree::new();
    two_files.skill("home/.agents/skills/one", "solo", Some("first"));
    two_files.skill("home/.agents/skills/two", "solo", Some("second"));
    let clash = load(&two_files.options("proj")).await;
    assert_eq!(names(&clash), expected_names(&["solo", "solo"]));
    assert_eq!(clash.named("solo").len(), 2);
    assert!(clash.warnings().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_alias_is_one_canonical_source() {
    let tree = Tree::new();
    tree.skill("home/.agents/skills/canonical", "canonical", Some("real"));
    fs::create_dir_all(tree.at("aliases")).expect("mkdir");
    std::os::unix::fs::symlink(
        tree.home().join(".agents/skills/canonical"),
        tree.at("aliases/canonical"),
    )
    .expect("symlink");

    let skills = load(&tree.options_with_paths(
        "proj",
        vec![tree.at("aliases").to_string_lossy().into_owned()],
    ))
    .await;

    assert_eq!(
        skills.named("canonical").len(),
        1,
        "aliases of one SKILL.md must not be advertised as ambiguous sources"
    );
    assert!(skills.get("canonical").is_some());
}

#[tokio::test]
async fn a_disk_skill_does_not_override_the_builtin() {
    let tree = Tree::new();
    tree.skill(
        &format!("home/.config/zuno/skill/{}", builtin::NAME),
        builtin::NAME,
        Some("mine"),
    );

    let skills = load(&tree.options("proj")).await;

    assert_eq!(names(&skills), expected_names(&[builtin::NAME]));
    assert!(skills.get(builtin::NAME).is_none());
    assert_eq!(skills.named(builtin::NAME).len(), 2);
    assert!(skills.warnings().is_empty());
}

#[tokio::test]
async fn a_description_less_skill_loads_but_never_renders() {
    let tree = Tree::new();
    tree.skill("home/.agents/skills/quiet", "quiet", None);

    let skills = load(&tree.options("proj")).await;

    assert!(skills.get("quiet").is_some());
    assert!(!skills.render(Form::List).contains("quiet"));
    assert!(!skills.render(Form::Verbose).contains("quiet"));
}

#[tokio::test]
async fn shared_sidecar_metadata_can_require_explicit_invocation() {
    let tree = Tree::new();
    tree.skill(
        "home/.agents/skills/release",
        "release",
        Some("Long release instructions."),
    );
    let sidecar = tree.sidecar(
        "home/.agents/skills/release",
        "openai",
        "interface:\n  display_name: Release Engineering\n  short_description: Promote verified artifacts\npolicy:\n  allow_implicit_invocation: false\n",
    );

    let skills = load(&tree.options("proj")).await;
    let release = skills.get("release").expect("release loads");

    assert_eq!(release.catalog_display_name(), "Release Engineering");
    assert_eq!(
        release.catalog_description(),
        Some("Promote verified artifacts")
    );
    assert_eq!(release.exposure, SkillExposure::Explicit);
    assert!(!release.is_searchable());
    assert!(
        !skills
            .render(Form::Index)
            .contains("<skill name=\"release\"")
    );
    assert_eq!(
        release.metadata_sources,
        vec![sidecar.to_string_lossy().into_owned()]
    );
    assert!(
        release
            .read_body()
            .await
            .expect("explicit Skill remains loadable")
            .contains("body of release")
    );
}

#[tokio::test]
async fn native_sidecar_overlays_shared_metadata_and_can_select_search_only() {
    let tree = Tree::new();
    tree.skill(
        "home/.config/zuno/skill/powerapps",
        "powerapps",
        Some("Long shared description."),
    );
    tree.sidecar(
        "home/.config/zuno/skill/powerapps",
        "openai",
        "interface:\n  display_name: Shared title\npolicy:\n  allow_implicit_invocation: false\n",
    );
    tree.sidecar(
        "home/.config/zuno/skill/powerapps",
        "zuno",
        "interface:\n  short_description: Search the Power Apps catalog\npolicy:\n  exposure: search\n",
    );

    let skills = load(&tree.options("proj")).await;
    let skill = skills.get("powerapps").expect("powerapps loads");

    assert_eq!(skill.catalog_display_name(), "Shared title");
    assert_eq!(
        skill.catalog_description(),
        Some("Search the Power Apps catalog")
    );
    assert_eq!(skill.exposure, SkillExposure::Search);
    assert!(skill.is_searchable());
    assert!(!skill.is_indexed());
    assert!(!skills.render(Form::Index).contains("powerapps"));
    assert!(skills.render(Form::List).contains("powerapps"));
}

#[tokio::test]
async fn malformed_sidecar_metadata_warns_without_dropping_the_skill() {
    let tree = Tree::new();
    tree.skill(
        "home/.agents/skills/usable",
        "usable",
        Some("Still usable."),
    );
    let sidecar = tree.sidecar(
        "home/.agents/skills/usable",
        "openai",
        "policy:\n  allow_implicit_invocation: no\n",
    );

    let skills = load(&tree.options("proj")).await;

    assert!(skills.get("usable").is_some());
    let warning = skills
        .warnings()
        .iter()
        .find(|warning| matches!(warning.kind(), SkillWarningKind::MetadataMalformed(_)))
        .expect("malformed metadata is visible");
    assert_eq!(warning.source(), sidecar.to_string_lossy());
}

#[tokio::test]
async fn ordered_path_rules_disable_a_subtree_and_reenable_one_exact_skill() {
    let tree = Tree::new();
    let kept = tree.skill("home/.agents/skills/powerapps/kept", "kept", Some("Kept."));
    let disabled = tree.skill(
        "home/.agents/skills/powerapps/disabled",
        "disabled",
        Some("Disabled."),
    );
    let root = tree.at("home/.agents/skills/powerapps");
    let options = tree.options("proj").with_path_config(vec![
        SkillPathConfig {
            path: root.to_string_lossy().into_owned(),
            enabled: Some(false),
            exposure: None,
            recursive: Some(true),
        },
        SkillPathConfig {
            path: kept
                .parent()
                .expect("skill directory")
                .to_string_lossy()
                .into_owned(),
            enabled: None,
            exposure: Some(SkillCatalogExposure::Explicit),
            recursive: None,
        },
    ]);

    let skills = load(&options).await;

    let kept = skills.get("kept").expect("last exact rule reenables");
    assert_eq!(kept.exposure, SkillExposure::Explicit);
    assert!(skills.get("disabled").is_none());
    assert_eq!(
        skills.disabled_sources(),
        [disabled.to_string_lossy().into_owned()]
    );
    assert!(skills.warnings().is_empty(), "{:?}", skills.warnings());
}

#[tokio::test]
async fn a_missing_path_rule_is_non_fatal_and_does_not_disable_other_skills() {
    let tree = Tree::new();
    tree.skill("home/.agents/skills/usable", "usable", Some("Usable."));
    let options = tree.options("proj").with_path_config(vec![SkillPathConfig {
        path: tree.at("future/skill").to_string_lossy().into_owned(),
        enabled: Some(false),
        exposure: None,
        recursive: None,
    }]);

    let skills = load(&options).await;

    assert!(skills.get("usable").is_some());
    assert!(skills.disabled_sources().is_empty());
    assert!(skills.warnings().is_empty(), "{:?}", skills.warnings());
}

#[cfg(unix)]
#[tokio::test]
async fn a_path_rule_matches_a_symlink_alias_of_the_skill_directory() {
    let tree = Tree::new();
    let real = tree.skill(
        "home/.agents/skills/canonical",
        "canonical",
        Some("Canonical."),
    );
    fs::create_dir_all(tree.at("aliases")).expect("alias parent");
    let alias = tree.at("aliases/canonical");
    std::os::unix::fs::symlink(real.parent().expect("skill directory"), &alias)
        .expect("skill alias");
    let options = tree.options("proj").with_path_config(vec![SkillPathConfig {
        path: alias.to_string_lossy().into_owned(),
        enabled: Some(false),
        exposure: None,
        recursive: None,
    }]);

    let skills = load(&options).await;

    assert!(skills.get("canonical").is_none());
    assert_eq!(
        skills.disabled_sources(),
        [real.to_string_lossy().into_owned()]
    );
}

#[tokio::test]
async fn a_broken_frontmatter_block_is_warned_about_and_skipped() {
    let tree = Tree::new();
    let broken = tree.write(
        "home/.agents/skills/bad/SKILL.md",
        "---\nname: [unclosed\n  - a: : :\n---\nB\n",
    );
    tree.skill("home/.agents/skills/good", "good", Some("d"));

    let skills = load(&tree.options("proj")).await;

    assert_eq!(names(&skills), expected_names(&["good"]));
    let warning = skills
        .warnings()
        .iter()
        .find(|w| matches!(w.kind(), SkillWarningKind::Frontmatter(_)))
        .expect("a frontmatter warning");
    assert_eq!(warning.source(), broken.to_string_lossy());
}

#[tokio::test]
async fn dirs_are_reported_for_every_match() {
    let tree = Tree::new();
    let one = tree.skill("home/.agents/skills/one", "one", Some("d"));
    let two = tree.skill("home/.config/zuno/skill/two", "two", Some("d"));

    let skills = load(&tree.options("proj")).await;

    assert_eq!(
        skills.dirs(),
        [
            two.parent().expect("parent").to_path_buf(),
            one.parent().expect("parent").to_path_buf()
        ]
    );
}

#[tokio::test]
async fn a_body_is_read_verbatim_after_selection() {
    let tree = Tree::new();
    tree.write(
        "home/.agents/skills/verbatim/SKILL.md",
        "---\nname: verbatim\ndescription: d\n---\n\n  indented\ttab\r\n\nlast\n",
    );

    let skills = load(&tree.options("proj")).await;

    assert_eq!(
        skills
            .get("verbatim")
            .expect("present")
            .read_body()
            .await
            .expect("body reads"),
        "\n  indented\ttab\r\n\nlast\n"
    );
}

#[test]
fn parse_file_reports_a_missing_name_as_an_invalid_config_naming_the_key() {
    let tree = Tree::new();
    let path = tree.write(
        "home/.agents/skills/anon/SKILL.md",
        "---\ndescription: d\n---\nB\n",
    );

    let error = parse_file(&path).expect_err("must reject");
    let ConfigError::Invalid { path: at, issues } = &error else {
        panic!("expected Invalid, got {error:?}");
    };
    assert_eq!(at, &path);
    assert_eq!(issues[0].key_path, vec!["name".to_string()]);
    assert!(
        error
            .to_string()
            .contains(&path.to_string_lossy().to_string())
    );
}

#[test]
fn parse_file_reports_an_absent_file_as_io_naming_the_path() {
    let tree = Tree::new();
    let missing = tree.at("nowhere/SKILL.md");
    let error = parse_file(&missing).expect_err("must fail");
    let ConfigError::Io { path, .. } = &error else {
        panic!("expected Io, got {error:?}");
    };
    assert_eq!(path, &missing);
}

#[test]
fn parse_file_round_trips_a_good_skill() {
    let tree = Tree::new();
    let path = tree.skill("home/.agents/skills/round", "round", Some("d"));
    assert_eq!(
        parse_file(&path).expect("loads"),
        Skill::file("round".to_string(), Some("d".to_string()), path.clone(),)
    );
}

/// Names the roots of an opt-in real-world corpus, `PATH`-separated.
const SKILL_CORPUS_ENV: &str = "ZUNO_SKILL_CORPUS";

/// An opt-in corpus check: every `SKILL.md` under the requested roots must parse.
///
/// This used to hardcode absolute host paths such as `/config/.agents/skills` and `continue`
/// past each one that was absent, so on every machine but one it walked nothing,
/// counted nothing, and passed. That is the unfalsifiable-gate shape this project
/// removed from `zuno-config/tests/differential.rs`, `plugin_models.rs` and
/// `live_sdk.rs`: a check whose subject lives outside the repository and whose
/// absence is indistinguishable from success.
///
/// The corpus itself is worth keeping — real authored skill files exercise
/// frontmatter forms the fixtures do not invent — so the check runs **only when
/// asked for**, and once asked for it is **fail-closed**.
/// Three states, all distinguishable from the outside:
///
/// * `ZUNO_SKILL_CORPUS` unset — announces a visible `SKIPPED` and asserts nothing.
/// * set to a root that is missing, is not a directory, or holds no `SKILL.md` —
///   **fails**, because a request that silently degrades into a pass is the
///   defect, not the fix.
/// * set to a real tree — every `SKILL.md` under it must parse and yield a name.
#[test]
fn parse_file_accepts_every_skill_in_an_opt_in_real_corpus() {
    let separator = if cfg!(windows) { ';' } else { ':' };
    let Some(raw) = std::env::var_os(SKILL_CORPUS_ENV) else {
        eprintln!(
            "SKIPPED parse_file_accepts_every_skill_in_an_opt_in_real_corpus: \
             {SKILL_CORPUS_ENV} is unset, so NO real skill tree was parsed on this host. Set it to \
             one or more `{separator}`-separated directories to run the corpus check; once \
             requested it fails rather than skips."
        );
        return;
    };

    let roots = std::env::split_paths(&raw)
        .filter(|root| !root.as_os_str().is_empty())
        .collect::<Vec<_>>();
    assert!(
        !roots.is_empty(),
        "{SKILL_CORPUS_ENV} is set to {raw:?}, which names no directory. An explicit request must \
         name a corpus; an empty request is how the old hardcoded-path version passed without \
         reading anything."
    );

    let mut checked = 0usize;
    for root in &roots {
        assert!(
            root.is_dir(),
            "{SKILL_CORPUS_ENV} names {}, which is not a directory. A live check that was asked \
             for must fail, not skip.",
            root.display()
        );
        let found = walk(root);
        assert!(
            !found.is_empty(),
            "{SKILL_CORPUS_ENV} names {}, which contains no SKILL.md. Walking an empty tree would \
             report success without parsing anything.",
            root.display()
        );
        for entry in found {
            let skill = parse_file(&entry)
                .unwrap_or_else(|error| panic!("{} must parse: {error}", entry.display()));
            assert!(
                !skill.name.is_empty(),
                "{} parsed to an empty name",
                entry.display()
            );
            checked += 1;
        }
    }

    assert!(
        checked > 0,
        "{SKILL_CORPUS_ENV} named {} root(s) but nothing was parsed",
        roots.len()
    );
    eprintln!("parsed {checked} real SKILL.md file(s) from {SKILL_CORPUS_ENV}");
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|name| name == "SKILL.md") {
                found.push(path);
            }
        }
    }
    found
}

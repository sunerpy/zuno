//! End-to-end [`oc_catalog::skill::load`] behaviour over real filesystem trees.
//!
//! The unit tests inside the crate cover each root and each frontmatter rule in
//! isolation. These cover the assembly: that the six roots compose in the right
//! order, that the built-in is displaced rather than duplicated, and that the two
//! de-duplication dimensions stay distinct.

use oc_catalog::skill::{
    Form, Skill, SkillOptions, SkillWarningKind, Skills, builtin, load, parse_file,
};
use oc_error::ConfigError;
use oc_paths::Env;
use oc_paths::env::{HOME, XDG_CACHE_HOME, XDG_CONFIG_HOME};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

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
        self.dir.path().join(relative)
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
}

fn names(skills: &Skills) -> Vec<String> {
    skills.all().iter().map(|s| s.name.clone()).collect()
}

fn located(skills: &Skills, name: &str) -> String {
    skills
        .get(name)
        .unwrap_or_else(|| panic!("{name} must be loaded"))
        .location
        .clone()
}

#[tokio::test]
async fn every_root_contributes_and_the_builtin_comes_first() {
    let tree = Tree::new();
    tree.skill("home/.claude/skills/from-claude", "from-claude", Some("c"));
    tree.skill("home/.agents/skills/from-agents", "from-agents", Some("a"));
    tree.skill(
        "proj/.agents/skills/from-project",
        "from-project",
        Some("p"),
    );
    tree.skill(
        "home/.config/opencode/skill/from-config",
        "from-config",
        Some("g"),
    );
    tree.skill(
        "home/.config/opencode/skills/from-config-plural",
        "from-config-plural",
        Some("gp"),
    );
    tree.skill("extra/from-path", "from-path", Some("x"));
    fs::create_dir_all(tree.at("proj/sub")).expect("mkdir");

    let skills = load(&tree.options_with_paths(
        "proj/sub",
        vec![tree.at("extra").to_string_lossy().into_owned()],
    ))
    .await;

    assert_eq!(
        names(&skills),
        vec![
            builtin::NAME.to_string(),
            "from-claude".to_string(),
            "from-agents".to_string(),
            "from-project".to_string(),
            "from-config".to_string(),
            "from-config-plural".to_string(),
            "from-path".to_string(),
        ]
    );
    assert!(skills.warnings().is_empty(), "{:?}", skills.warnings());
}

#[tokio::test]
async fn a_skill_file_with_no_name_is_rejected_and_the_warning_names_the_file() {
    let tree = Tree::new();
    let broken = tree.write(
        "home/.config/opencode/skill/broken/SKILL.md",
        "---\ndescription: I forgot my name.\n---\n\nBody.\n",
    );
    tree.skill("home/.config/opencode/skill/fine", "fine", Some("d"));

    let skills = load(&tree.options("proj")).await;

    assert_eq!(
        names(&skills),
        vec![builtin::NAME.to_string(), "fine".to_string()]
    );
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
async fn a_duplicate_name_emits_exactly_one_warning_and_the_later_root_wins() {
    let tree = Tree::new();
    tree.skill("home/.claude/skills/dupe", "dupe", Some("from claude"));
    tree.skill("home/.agents/skills/dupe", "dupe", Some("from agents"));

    let skills = load(&tree.options("proj")).await;

    let duplicates: Vec<&_> = skills
        .warnings()
        .iter()
        .filter(|warning| matches!(warning.kind(), SkillWarningKind::DuplicateName { .. }))
        .collect();
    eprintln!(
        "QA failure scenario 2 -- {} duplicate warning(s): {}",
        duplicates.len(),
        duplicates
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join(" | ")
    );
    assert_eq!(duplicates.len(), 1, "{:?}", skills.warnings());
    assert_eq!(
        duplicates[0].kind(),
        &SkillWarningKind::DuplicateName {
            name: "dupe".to_string(),
            existing: tree
                .at("home/.claude/skills/dupe/SKILL.md")
                .to_string_lossy()
                .into_owned(),
        }
    );
    assert_eq!(
        located(&skills, "dupe"),
        tree.at("home/.agents/skills/dupe/SKILL.md")
            .to_string_lossy()
    );
    assert_eq!(
        skills.get("dupe").expect("present").description.as_deref(),
        Some("from agents")
    );
}

#[tokio::test]
async fn a_config_directory_beats_both_external_roots() {
    let tree = Tree::new();
    tree.skill("home/.claude/skills/dupe", "dupe", Some("from claude"));
    tree.skill("home/.agents/skills/dupe", "dupe", Some("from agents"));
    tree.skill(
        "home/.config/opencode/skill/dupe",
        "dupe",
        Some("from config"),
    );

    let skills = load(&tree.options("proj")).await;

    assert_eq!(
        located(&skills, "dupe"),
        tree.at("home/.config/opencode/skill/dupe/SKILL.md")
            .to_string_lossy()
    );
    assert_eq!(
        skills
            .warnings()
            .iter()
            .filter(|w| matches!(w.kind(), SkillWarningKind::DuplicateName { .. }))
            .count(),
        2,
        "one warning per displacement"
    );
}

/// The two de-duplication dimensions, side by side.
///
/// Left: one file, two roots -> one match, **no** duplicate warning. Right: two
/// files, one name -> two matches, **one** duplicate warning. Confusing the two
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
    assert_eq!(
        names(&reached_twice),
        vec![builtin::NAME.to_string(), "solo".to_string()]
    );
    assert!(
        reached_twice.warnings().is_empty(),
        "the same path through two roots is not a duplicate name: {:?}",
        reached_twice.warnings()
    );

    let two_files = Tree::new();
    two_files.skill("home/.agents/skills/one", "solo", Some("first"));
    two_files.skill("home/.agents/skills/two", "solo", Some("second"));
    let clash = load(&two_files.options("proj")).await;
    assert_eq!(
        names(&clash),
        vec![builtin::NAME.to_string(), "solo".to_string()]
    );
    assert_eq!(clash.warnings().len(), 1);
}

#[tokio::test]
async fn a_symlink_alias_is_a_duplicate_name_not_a_duplicate_path() {
    let tree = Tree::new();
    tree.skill("home/.agents/skills/aliased", "aliased", Some("real"));
    fs::create_dir_all(tree.home().join(".claude/skills")).expect("mkdir");
    std::os::unix::fs::symlink(
        tree.home().join(".agents/skills/aliased"),
        tree.home().join(".claude/skills/aliased"),
    )
    .expect("symlink");

    let skills = load(&tree.options("proj")).await;

    assert_eq!(skills.warnings().len(), 1, "{:?}", skills.warnings());
    assert!(matches!(
        skills.warnings()[0].kind(),
        SkillWarningKind::DuplicateName { .. }
    ));
    // The real path wins because `.agents` is scanned after `.claude`, which is
    // what the oracle reports for every alias on the surveyed machine.
    assert_eq!(
        located(&skills, "aliased"),
        tree.at("home/.agents/skills/aliased/SKILL.md")
            .to_string_lossy()
    );
}

#[tokio::test]
async fn a_disk_skill_overrides_the_builtin_with_one_warning() {
    let tree = Tree::new();
    tree.skill(
        &format!("home/.config/opencode/skill/{}", builtin::NAME),
        builtin::NAME,
        Some("mine"),
    );

    let skills = load(&tree.options("proj")).await;

    assert_eq!(names(&skills), vec![builtin::NAME.to_string()]);
    assert_eq!(
        skills
            .get(builtin::NAME)
            .expect("present")
            .description
            .as_deref(),
        Some("mine")
    );
    assert_eq!(skills.warnings().len(), 1);
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
async fn a_broken_frontmatter_block_is_warned_about_and_skipped() {
    let tree = Tree::new();
    let broken = tree.write(
        "home/.agents/skills/bad/SKILL.md",
        "---\nname: [unclosed\n  - a: : :\n---\nB\n",
    );
    tree.skill("home/.agents/skills/good", "good", Some("d"));

    let skills = load(&tree.options("proj")).await;

    assert_eq!(
        names(&skills),
        vec![builtin::NAME.to_string(), "good".to_string()]
    );
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
    let two = tree.skill("home/.config/opencode/skill/two", "two", Some("d"));

    let skills = load(&tree.options("proj")).await;

    assert_eq!(
        skills.dirs(),
        [
            one.parent().expect("parent").to_path_buf(),
            two.parent().expect("parent").to_path_buf()
        ]
    );
}

#[tokio::test]
async fn a_body_is_loaded_verbatim() {
    let tree = Tree::new();
    tree.write(
        "home/.agents/skills/verbatim/SKILL.md",
        "---\nname: verbatim\ndescription: d\n---\n\n  indented\ttab\r\n\nlast\n",
    );

    let skills = load(&tree.options("proj")).await;

    assert_eq!(
        skills.get("verbatim").expect("present").content,
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
        Skill {
            name: "round".to_string(),
            description: Some("d".to_string()),
            location: path.to_string_lossy().into_owned(),
            content: "\nbody of round\n".to_string(),
        }
    );
}

#[test]
fn parse_file_accepts_the_users_real_skill_files() {
    // A cheap corpus check: every SKILL.md the surveyed machine ships must parse.
    // Read-only; skipped when the tree is not present.
    let roots = [
        Path::new("/config/.config/opencode/skill"),
        Path::new("/config/.agents/skills"),
    ];
    let mut checked = 0usize;
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        for entry in walk(root) {
            let skill = parse_file(&entry)
                .unwrap_or_else(|error| panic!("{} must parse: {error}", entry.display()));
            assert!(!skill.name.is_empty());
            checked += 1;
        }
    }
    if checked == 0 {
        eprintln!("skipped: no real skill tree on this host");
    }
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

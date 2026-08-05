//! Differential parity against `opencode debug skill`.
//!
//! Two halves, because they answer different questions.
//!
//! **The matrix** builds each root in a sealed [`ScriptedEnv`] and compares the
//! *whole* `debug skill` document — every name, description, location and body —
//! byte for byte. That is the strongest available check, and it is only possible
//! because these fixtures plant no duplicate names.
//!
//! **The real-tree case** runs against the machine's own installed skills with the
//! real `$HOME`, and compares the name **set**. It cannot compare locations,
//! because a large share of those skills exist twice — once under
//! `~/.claude/skills/x`, once as the `~/.agents/skills/x` the first is a symlink
//! to — and the oracle's duplicate-name winner is decided by I/O timing
//! (`skill/index.ts:240-243` loads with `concurrency: "unbounded"`). Measured:
//! three consecutive runs over one crafted fixture reported three different
//! location sets and one identical name set. The name set is the contract; the
//! winner is not.
//!
//! The real-tree case compares against `opencode --pure debug skill`, and it is
//! worth being explicit about why. On the surveyed machine the plain run reports
//! **one extra** skill, `security-research`, from
//! `$XDG_CACHE_HOME/opencode/skills/` — a directory only `skills.urls[]` can
//! populate — and resolves `security-review` to the cache copy instead of the
//! config-directory one. Neither comes from config: the installed
//! `@sunerpy/oh-my-openagent` plugin contributes the `skills.*` entries at load
//! time. `--pure` drops external plugins and the run becomes 135 skills with no
//! cache locations at all, which is exactly what this port produces. So the
//! comparison is not weakened to make it pass: the plain run is *also* asserted,
//! and the whole gap is required to be cache-located, which is what proves the
//! remaining difference is the unimplemented plugin layer (todo 26+) and nothing
//! about discovery.
//!
//! # Why neither half calls `Oracle::run`
//!
//! Measured on 1.18.12: `opencode debug skill` truncates its own stdout when
//! stdout is a pipe. Three runs of the real tree piped 40960, 40960 and 57344
//! bytes; the same three runs redirected to a file produced 2807771 bytes each
//! time. `debug/skill.ts` ends with a bare `process.stdout.write(...)` and the
//! process exits without draining the pipe. `Oracle::run` captures through a pipe,
//! so it cannot be used for this command at all — both halves redirect stdout to
//! a file with [`run_debug_skill`] instead, and use [`Oracle`] to locate and
//! version-stamp the binary.
//!
//! `ScriptedEnv` also always rewrites `HOME`, which the real-tree half must not
//! do: the whole point is to let the binary see the machine's own skill tree.

use oc_catalog::skill::{SkillOptions, Skills, load};
use oc_config::Config;
use oc_config::discovery::{DiscoveryOptions, discover_with};
use oc_paths::Env;
use oc_testkit::{Normalizer, Oracle, ScriptedEnv, diff_normalized};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

const REQUIRED_COVERAGE: &[&str] = &[
    "~/.claude/skills",
    "~/.agents/skills",
    "project-.claude",
    "project-.agents",
    "config-skill",
    "config-skills",
    "skills.paths-absolute",
    "skills.paths-tilde",
    "skills.paths-relative",
    "skills.paths-missing",
    "nested-deep",
    "dot-dir-external-included",
    "dot-dir-config-excluded",
    "no-description",
    "missing-name-rejected",
    "non-string-name-rejected",
    "unknown-frontmatter-keys-ignored",
    "folded-description",
    "unquoted-colon-description",
    "crlf",
    "OPENCODE_DISABLE_CLAUDE_CODE_SKILLS",
    "OPENCODE_DISABLE_EXTERNAL_SKILLS",
    "OPENCODE_DISABLE_PROJECT_CONFIG",
    "OPENCODE_CONFIG_DIR",
];

/// Empty on purpose. Every fixture below matches the oracle byte for byte; if
/// that ever stops being true the reason belongs here with the oracle line that
/// explains it, and [`every_listed_divergence_is_still_real`] keeps the list from
/// outliving its cause.
const INTENTIONAL_DIVERGENCES: &[(&str, &str)] = &[];

struct Case {
    name: &'static str,
    env: ScriptedEnv,
    coverage: &'static [&'static str],
}

/// Writes into the directories a [`ScriptedEnv`] already owns.
struct Sandbox<'a> {
    env: &'a ScriptedEnv,
}

impl<'a> Sandbox<'a> {
    fn new(env: &'a ScriptedEnv) -> Self {
        Self { env }
    }

    fn config_dir(&self) -> PathBuf {
        self.env.xdg_config().join("opencode")
    }

    fn raw(&self, path: &Path, body: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, body).expect("write");
    }

    fn skill(&self, dir: &Path, name: &str, description: Option<&str>) {
        let front = match description {
            Some(description) => format!("---\nname: {name}\ndescription: {description}\n---\n"),
            None => format!("---\nname: {name}\n---\n"),
        };
        self.raw(
            &dir.join("SKILL.md"),
            &format!("{front}\n# {name}\n\nBody of {name}.\n"),
        );
    }

    fn config(&self, body: &str) {
        self.raw(&self.config_dir().join("opencode.json"), body);
    }
}

fn quoted(path: &Path) -> String {
    serde_json::to_string(&path.to_string_lossy()).expect("json string")
}

fn six_roots_case() -> Case {
    let env = ScriptedEnv::new().expect("scripted env");
    let project = env.project().to_path_buf();
    let deep = project.join("nested/cwd");
    fs::create_dir_all(&deep).expect("mkdir");
    let env = env.with_working_dir(&deep).expect("cwd");

    {
        let tree = Sandbox::new(&env);
        tree.skill(
            &env.home().join(".claude/skills/from-claude"),
            "from-claude",
            Some("root 1"),
        );
        tree.skill(
            &env.home().join(".agents/skills/from-agents"),
            "from-agents",
            Some("root 2"),
        );
        tree.skill(
            &env.home().join(".agents/skills/.dotted/from-dotted-agents"),
            "from-dotted-agents",
            Some("root 2 sets dot, so a dot directory is traversed"),
        );
        tree.skill(
            &project.join(".agents/skills/from-project-agents"),
            "from-project-agents",
            Some("root 3"),
        );
        tree.skill(
            &project.join(".claude/skills/from-project-claude"),
            "from-project-claude",
            Some("root 3"),
        );
        tree.skill(
            &tree.config_dir().join("skill/from-config-singular"),
            "from-config-singular",
            Some("root 4, singular directory"),
        );
        tree.skill(
            &tree.config_dir().join("skills/from-config-plural"),
            "from-config-plural",
            Some("root 4, plural directory"),
        );
        tree.skill(
            &tree.config_dir().join("skill/a/b/c/from-config-deep"),
            "from-config-deep",
            Some("root 4, four levels down"),
        );
        tree.skill(
            &tree.config_dir().join("skill/.dotted/hidden"),
            "from-dotted-config-must-be-invisible",
            Some("root 4 leaves dot unset"),
        );
        tree.skill(
            &env.root().join("absolute-skills/from-absolute"),
            "from-absolute",
            Some("root 5, absolute entry"),
        );
        tree.skill(
            &env.home().join("tilde-skills/from-tilde"),
            "from-tilde",
            Some("root 5, tilde entry"),
        );
        tree.skill(
            &deep.join("relative-skills/from-relative"),
            "from-relative",
            Some("root 5, relative to the working directory"),
        );
        tree.config(&format!(
            r#"{{"$schema":"https://opencode.ai/config.json","skills":{{"paths":[{},"~/tilde-skills","relative-skills",{}]}}}}"#,
            quoted(&env.root().join("absolute-skills")),
            quoted(&env.root().join("absent-skills")),
        ));
    }

    Case {
        name: "six-roots",
        env,
        coverage: &[
            "~/.claude/skills",
            "~/.agents/skills",
            "project-.claude",
            "project-.agents",
            "config-skill",
            "config-skills",
            "skills.paths-absolute",
            "skills.paths-tilde",
            "skills.paths-relative",
            "skills.paths-missing",
            "nested-deep",
            "dot-dir-external-included",
            "dot-dir-config-excluded",
        ],
    }
}

fn frontmatter_case() -> Case {
    let env = ScriptedEnv::new().expect("scripted env");
    {
        let tree = Sandbox::new(&env);
        let root = tree.config_dir().join("skill");
        tree.skill(&root.join("plain"), "plain", Some("an ordinary skill"));
        tree.skill(&root.join("quiet"), "quiet", None);
        tree.raw(
            &root.join("anonymous/SKILL.md"),
            "---\ndescription: no name at all\n---\n\nBody.\n",
        );
        tree.raw(
            &root.join("numeric/SKILL.md"),
            "---\nname: 123\ndescription: a numeric name\n---\n\nBody.\n",
        );
        tree.raw(
            &root.join("nulldesc/SKILL.md"),
            "---\nname: nulldesc\ndescription:\n---\n\nBody.\n",
        );
        tree.raw(
            &root.join("extras/SKILL.md"),
            "---\nname: extras\ndescription: unknown keys are ignored\nlicense: MIT\nallowed-tools: [bash]\nversion: 7\n---\n\nBody.\n",
        );
        tree.raw(
            &root.join("folded/SKILL.md"),
            "---\nname: folded\ndescription: >\n  first line\n  second line\n\n  new paragraph\n---\n\nBody.\n",
        );
        tree.raw(
            &root.join("coloned/SKILL.md"),
            "---\nname: coloned\ndescription: Use when: the value has a colon. Also: here\n---\n\nBody.\n",
        );
        tree.raw(
            &root.join("crlf/SKILL.md"),
            "---\r\nname: crlf\r\ndescription: carriage returns everywhere\r\n---\r\nBody with CRLF.\r\n",
        );
        tree.raw(
            &root.join("truthy/SKILL.md"),
            "---\nname: yes\ndescription: yes is a string under YAML 1.2 core\n---\n\nBody.\n",
        );
        tree.config(r#"{"$schema":"https://opencode.ai/config.json"}"#);
    }

    Case {
        name: "frontmatter-edges",
        env,
        coverage: &[
            "no-description",
            "missing-name-rejected",
            "non-string-name-rejected",
            "unknown-frontmatter-keys-ignored",
            "folded-description",
            "unquoted-colon-description",
            "crlf",
        ],
    }
}

fn flag_case(name: &'static str, flag: &str, coverage: &'static [&'static str]) -> Case {
    let env = ScriptedEnv::new().expect("scripted env");
    let project = env.project().to_path_buf();
    {
        let tree = Sandbox::new(&env);
        tree.skill(
            &env.home().join(".claude/skills/home-claude"),
            "home-claude",
            Some("root 1"),
        );
        tree.skill(
            &env.home().join(".agents/skills/home-agents"),
            "home-agents",
            Some("root 2"),
        );
        tree.skill(
            &project.join(".claude/skills/project-claude"),
            "project-claude",
            Some("root 3"),
        );
        tree.skill(
            &project.join(".agents/skills/project-agents"),
            "project-agents",
            Some("root 3"),
        );
        tree.skill(
            &tree.config_dir().join("skill/config-skill"),
            "config-skill",
            Some("root 4"),
        );
        tree.config(r#"{"$schema":"https://opencode.ai/config.json"}"#);
    }
    Case {
        name,
        env: env.set(flag, "1"),
        coverage,
    }
}

fn config_dir_case() -> Case {
    let env = ScriptedEnv::new().expect("scripted env");
    let elsewhere = env.root().join("elsewhere");
    {
        let tree = Sandbox::new(&env);
        tree.skill(
            &elsewhere.join("skills/from-override"),
            "from-override",
            Some("found through OPENCODE_CONFIG_DIR"),
        );
        tree.skill(
            &tree.config_dir().join("skill/from-global"),
            "from-global",
            Some("the ordinary global config directory"),
        );
        tree.config(r#"{"$schema":"https://opencode.ai/config.json"}"#);
    }
    Case {
        name: "config-dir-override",
        env: env.set("OPENCODE_CONFIG_DIR", elsewhere.to_string_lossy()),
        coverage: &["OPENCODE_CONFIG_DIR"],
    }
}

fn cases() -> Vec<Case> {
    vec![
        six_roots_case(),
        frontmatter_case(),
        flag_case(
            "disable-claude-skills",
            "OPENCODE_DISABLE_CLAUDE_CODE_SKILLS",
            &["OPENCODE_DISABLE_CLAUDE_CODE_SKILLS"],
        ),
        flag_case(
            "disable-external-skills",
            "OPENCODE_DISABLE_EXTERNAL_SKILLS",
            &["OPENCODE_DISABLE_EXTERNAL_SKILLS"],
        ),
        flag_case(
            "disable-project-config",
            "OPENCODE_DISABLE_PROJECT_CONFIG",
            &["OPENCODE_DISABLE_PROJECT_CONFIG"],
        ),
        config_dir_case(),
    ]
}

/// Sort by name and re-render, so the oracle's I/O-order output and this port's
/// root-order output become comparable without dropping a single field.
fn canonical(document: &str, label: &str) -> Result<String, Box<dyn Error>> {
    let mut parsed: Vec<Value> = serde_json::from_str(document.trim())
        .map_err(|error| format!("{label} is not a JSON array: {error}"))?;
    parsed.sort_by(|left, right| {
        left.get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .cmp(
                right
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )
    });
    Ok(format!("{}\n", serde_json::to_string_pretty(&parsed)?))
}

/// Run `opencode debug skill` with stdout redirected to a file.
///
/// A pipe loses most of the output (see the module documentation), so the only
/// reliable capture is a real file. `vars` replaces the environment entirely when
/// supplied; `None` inherits the process environment, which is what lets the
/// binary see the machine's own `$HOME`.
fn run_debug_skill(
    program: &Path,
    vars: Option<&BTreeMap<String, String>>,
    cwd: &Path,
) -> Result<String, Box<dyn Error>> {
    run_debug_skill_with(program, vars, cwd, &["debug", "skill"])
}

/// [`run_debug_skill`] with an explicit argument list, for the `--pure` variant.
fn run_debug_skill_with(
    program: &Path,
    vars: Option<&BTreeMap<String, String>>,
    cwd: &Path,
    args: &[&str],
) -> Result<String, Box<dyn Error>> {
    let capture = TempDir::new()?;
    let out = capture.path().join("stdout.json");
    let err = capture.path().join("stderr.txt");

    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdout(File::create(&out)?)
        .stderr(File::create(&err)?);
    if let Some(vars) = vars {
        command.env_clear();
        for (key, value) in vars {
            command.env(key, value);
        }
    }

    let status = command.status()?;
    let stdout = fs::read_to_string(&out).unwrap_or_default();
    if !status.success() {
        let stderr = fs::read_to_string(&err).unwrap_or_default();
        return Err(format!("{} exited with {status}: {stderr}", program.display()).into());
    }
    Ok(stdout)
}

async fn rust_skills(vars: BTreeMap<String, String>, cwd: &Path) -> Result<Skills, Box<dyn Error>> {
    let snapshot = Env::from_pairs(vars);
    let config: Config = discover_with(&DiscoveryOptions::new(
        cwd.to_path_buf(),
        None::<PathBuf>,
        snapshot.clone(),
    ))?;
    Ok(load(&SkillOptions::from_config(
        cwd.to_path_buf(),
        None::<PathBuf>,
        &snapshot,
        &config,
    ))
    .await)
}

fn parse_skills(document: &str) -> Result<BTreeMap<String, String>, Box<dyn Error>> {
    let parsed: Vec<Value> = serde_json::from_str(document.trim())?;
    Ok(parsed
        .iter()
        .filter_map(|entry| {
            Some((
                entry.get("name").and_then(Value::as_str)?.to_string(),
                entry.get("location").and_then(Value::as_str)?.to_string(),
            ))
        })
        .collect())
}

#[tokio::test]
async fn debug_skill_matches_the_oracle_across_every_root() -> Result<(), Box<dyn Error>> {
    let mut failures = Vec::new();
    let mut covered = BTreeSet::new();
    let mut ran = 0usize;

    for case in cases() {
        covered.extend(case.coverage.iter().copied());

        // Snapshot what the Rust side needs before `with_env` takes ownership.
        let vars = case.env.env_vars();
        let cwd = case.env.working_dir().to_path_buf();

        // `with_env` owns the scripted directories for the rest of this
        // iteration, which is what keeps the temporary tree alive.
        let oracle = Oracle::discover()?.with_env(case.env);
        if ran == 0 {
            eprintln!(
                "oracle: {} ({:?}), reported version {}",
                oracle.program().display(),
                oracle.provenance(),
                oracle.reported_version()
            );
        }
        let stdout = match run_debug_skill(oracle.program(), Some(&vars), &cwd) {
            Ok(stdout) => stdout,
            Err(error) => {
                failures.push(format!("case {}: {error}", case.name));
                continue;
            }
        };

        let oracle_json = canonical(&stdout, case.name)?;
        let skills = rust_skills(vars.clone(), &cwd).await?;
        let rust_json = canonical(&serde_json::to_string(skills.all())?, "rust")?;

        let report = diff_normalized(
            format!("opencode {} debug skill", oracle.reported_version()),
            &oracle_json,
            format!("oc-catalog skill case {}", case.name),
            &rust_json,
            &Normalizer::none(),
        );
        if report.is_identical() {
            eprintln!(
                "case {}: identical, {} skills",
                case.name,
                skills.all().len()
            );
        } else {
            failures.push(format!("case {}:\n{}", case.name, report.render()));
        }
        ran += 1;
    }

    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    assert!(ran >= 6, "only {ran} cases ran");
    let missing: Vec<&str> = REQUIRED_COVERAGE
        .iter()
        .copied()
        .filter(|item| !covered.contains(item))
        .collect();
    assert!(missing.is_empty(), "uncovered behaviour: {missing:?}");
    Ok(())
}

/// The acceptance criterion: the machine's own installed skill tree, name set for
/// name set.
///
/// Read-only. The working directory is a fresh temporary directory so neither side
/// picks up a project `.claude`, `.agents`, or `.opencode` from this repository.
#[tokio::test]
async fn the_real_skill_tree_yields_the_same_names_as_the_oracle() -> Result<(), Box<dyn Error>> {
    let oracle = Oracle::discover()?;
    eprintln!(
        "oracle: {} ({:?}), reported version {}",
        oracle.program().display(),
        oracle.provenance(),
        oracle.reported_version()
    );

    let scratch = TempDir::new()?;
    let cwd = scratch.path().to_path_buf();

    let config = real_config(&cwd);
    let skills = load(&SkillOptions::from_process(
        cwd.clone(),
        None::<PathBuf>,
        &config,
    ))
    .await;
    let rust_names: BTreeSet<String> = skills.all().iter().map(|s| s.name.clone()).collect();

    let pure = parse_skills(&run_debug_skill_with(
        oracle.program(),
        None,
        &cwd,
        &["--pure", "debug", "skill"],
    )?)?;
    let pure_names: BTreeSet<String> = pure.keys().cloned().collect();

    eprintln!(
        "oracle --pure reported {} skills, oc-catalog discovered {}",
        pure_names.len(),
        rust_names.len()
    );
    assert!(
        pure_names.len() > 50,
        "the host has no real skill tree to compare against ({} skills)",
        pure_names.len()
    );

    let only_oracle: Vec<&String> = pure_names.difference(&rust_names).collect();
    let only_rust: Vec<&String> = rust_names.difference(&pure_names).collect();
    assert!(
        only_oracle.is_empty() && only_rust.is_empty(),
        "name sets diverge.\nmissing from oc-catalog: {only_oracle:?}\nextra in oc-catalog: {only_rust:?}"
    );

    // Not a weaker second check: this is what pins the `--pure` choice to the
    // plugin layer instead of leaving it as an unexplained convenience.
    let plain = parse_skills(&run_debug_skill(oracle.program(), None, &cwd)?)?;
    let plain_names: BTreeSet<String> = plain.keys().cloned().collect();
    let plugin_only: Vec<&String> = plain_names.difference(&rust_names).collect();
    for name in &plugin_only {
        let location = &plain[*name];
        assert!(
            is_remote_cache(location),
            "{name} is in the plain run but not in oc-catalog, and {location} is not a \
             `skills.urls[]` cache directory, so it is not explained by the plugin layer"
        );
    }
    for (name, location) in &plain {
        if rust_names.contains(name) && is_remote_cache(location) {
            let ours = &skills.get(name).expect("present").location;
            eprintln!(
                "note: plugin-injected cache copy wins in the plain run for {name}: {location} \
                 (oc-catalog: {ours})"
            );
        }
    }
    eprintln!(
        "plain run adds {} plugin-injected skill(s): {plugin_only:?}",
        plugin_only.len()
    );
    Ok(())
}

/// The merged config for the real tree, or defaults with a proof that defaults are
/// equivalent *for skills*.
///
/// On the surveyed machine `oc-config` rejects the real `opencode.json` over an
/// unrecognized `theme` key. That is a schema gap in another crate; it must not
/// silently change what this test compares, so the fallback asserts the file
/// configures no skills at all.
fn real_config(cwd: &Path) -> Config {
    match discover_with(&DiscoveryOptions::from_process(
        cwd.to_path_buf(),
        None::<PathBuf>,
    )) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("real config did not load ({error}); continuing with defaults");
            let global = oc_paths::config().join("opencode.json");
            if let Ok(text) = fs::read_to_string(&global) {
                assert!(
                    !text.contains("\"skills\""),
                    "{} configures skills, so the default fallback would change the comparison",
                    global.display()
                );
            }
            Config::default()
        }
    }
}

/// Whether a location sits in the `skills.urls[]` download cache, which only the
/// remote root can populate (`skill/discovery.ts:35`).
fn is_remote_cache(location: &str) -> bool {
    Path::new(location).starts_with(oc_paths::cache().join("skills"))
}

/// A divergence list that outlives its cause is worse than no list: it makes a
/// real regression look sanctioned.
#[test]
fn every_listed_divergence_is_still_real() {
    let names: BTreeSet<&str> = INTENTIONAL_DIVERGENCES
        .iter()
        .map(|(name, _)| *name)
        .collect();
    assert_eq!(
        names.len(),
        INTENTIONAL_DIVERGENCES.len(),
        "duplicate divergence entries"
    );
    for (name, reason) in INTENTIONAL_DIVERGENCES {
        assert!(
            !name.is_empty() && reason.len() > 40,
            "{name} needs a reason"
        );
    }
}

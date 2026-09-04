//! On-disk agent discovery: the name rule, the body-to-prompt rule, and the
//! deprecated-key rejection, exercised against real directory trees.
//!
//! The unit tests in `src/agent.rs` cover the pure fold. These cover what only a
//! filesystem can: the `{agent,agents}` roots, nested paths, dot-directories,
//! symlinks, and the two QA scenarios named in this task's plan entry.

use std::error::Error;
use std::fs;
use std::path::Path;
use zuno_catalog::agent::{
    self, AgentMode, AgentSource, discover_in_directory, load_map, read_markdown_agent,
};
use zuno_config::discovery::{DiscoveryOptions, discover_with};
use zuno_config::schema::agent::AgentConfig;
use zuno_config::schema::ordered::OrderedMap;
use zuno_config::schema::permission::{PermissionAction, PermissionConfig, permission_key};
use zuno_error::ConfigError;
use zuno_paths::Env;
use zuno_permission::{evaluate, rules_from_config};

fn write(root: &Path, relative: &str, contents: &str) -> Result<(), Box<dyn Error>> {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, contents)?;
    Ok(())
}

fn to_map(
    agents: Vec<agent::MarkdownAgent>,
) -> (OrderedMap<AgentConfig>, Vec<(String, std::path::PathBuf)>) {
    let mut map = OrderedMap::new();
    let mut paths = Vec::new();
    for found in agents {
        map.insert(found.name.clone(), found.config);
        paths.push((found.name, found.path));
    }
    (map, paths)
}

#[test]
fn a_nested_markdown_agent_is_named_by_its_path_and_bodied_by_its_prompt()
-> Result<(), Box<dyn Error>> {
    // The happy QA scenario from the plan entry, and the rule this task exists to
    // get right. Verified against opencode 1.18.12, which printed
    // `review/security (subagent)` for exactly this file.
    let dir = tempfile::tempdir()?;
    write(
        dir.path(),
        "agent/review/security.md",
        "---\ndescription: Reviews code for security problems\nmode: subagent\n---\n\
         You are a security reviewer.\n\nLook for injection flaws.\n",
    )?;

    let found = discover_in_directory(dir.path())?;
    assert_eq!(found.len(), 1, "exactly one agent should be discovered");
    assert_eq!(
        found[0].name, "review/security",
        "the name is the path minus `agent/` and `.md`, not the basename"
    );

    let (map, paths) = to_map(found);
    let agents = agent::list(&map, &paths);
    let agent = agents
        .iter()
        .find(|agent| agent.name == "review/security")
        .expect("the discovered agent should resolve");
    assert_eq!(agent.mode, AgentMode::Subagent);
    assert_eq!(
        agent.description.as_deref(),
        Some("Reviews code for security problems")
    );
    assert_eq!(
        agent.prompt.as_deref(),
        Some("You are a security reviewer.\n\nLook for injection flaws."),
        "the trimmed body becomes the prompt"
    );
    assert_eq!(agent.header(), "review/security (subagent)");
    assert!(matches!(agent.source, AgentSource::Markdown { .. }));
    Ok(())
}

#[test]
fn a_markdown_agent_with_max_steps_is_rejected_with_a_message_naming_steps()
-> Result<(), Box<dyn Error>> {
    // The failure QA scenario from the plan entry. Todo 10's `legacy.rs` owns the
    // rejection; this asserts the agent loader actually calls it rather than
    // letting a deprecated key through into provider options.
    let dir = tempfile::tempdir()?;
    write(
        dir.path(),
        "agent/slow.md",
        "---\ndescription: capped\nmaxSteps: 5\n---\nBody.\n",
    )?;

    let error = discover_in_directory(dir.path()).expect_err("maxSteps must be rejected");
    let ConfigError::Invalid { path, issues } = &error else {
        panic!("expected a validation failure, got {error:?}");
    };
    assert!(
        path.ends_with("agent/slow.md"),
        "the error must name the file, got {}",
        path.display()
    );
    let rendered = issues
        .iter()
        .map(|issue| issue.detail.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rendered.contains("steps"),
        "the message must name `steps` as the replacement, got: {rendered}"
    );
    assert!(
        rendered.contains("maxSteps"),
        "the message must name what was found, got: {rendered}"
    );
    Ok(())
}

#[test]
fn a_markdown_agent_tools_must_be_a_sequence() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    write(
        dir.path(),
        "agent/toolsy.md",
        "---\ntools:\n  write: false\n---\nBody.\n",
    )?;
    let error = discover_in_directory(dir.path()).expect_err("tools must be rejected");
    let ConfigError::Invalid { issues, .. } = &error else {
        panic!("expected a validation failure, got {error:?}");
    };
    let rendered = issues
        .iter()
        .map(|issue| issue.detail.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rendered.contains("sequence"),
        "the message must explain that `tools` is a name sequence, got: {rendered}"
    );
    Ok(())
}

#[test]
fn markdown_agent_required_skills_reach_the_resolved_agent() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    write(
        dir.path(),
        "agent/indexer.md",
        "---\nmode: subagent\nrequiredSkills:\n  - codegraph\n  - review\n---\nBody.\n",
    )?;

    let found = discover_in_directory(dir.path())?;
    assert_eq!(
        found[0].config.required_skills.as_deref(),
        Some(["codegraph".to_owned(), "review".to_owned()].as_slice())
    );

    let (map, paths) = to_map(found);
    let agents = agent::list(&map, &paths);
    let indexer = agents
        .iter()
        .find(|candidate| candidate.name == "indexer")
        .expect("Markdown Agent resolves");
    assert_eq!(
        indexer.required_skills.as_deref(),
        Some(["codegraph".to_owned(), "review".to_owned()].as_slice())
    );
    Ok(())
}

#[test]
fn both_agent_and_agents_directories_are_scanned() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    write(dir.path(), "agent/from-singular.md", "---\n---\nA.\n")?;
    write(dir.path(), "agents/from-plural.md", "---\n---\nB.\n")?;

    let names: Vec<String> = discover_in_directory(dir.path())?
        .into_iter()
        .map(|found| found.name)
        .collect();
    assert_eq!(names, vec!["from-singular", "from-plural"]);
    Ok(())
}

#[test]
fn mode_and_modes_directories_are_not_scanned() -> Result<(), Box<dyn Error>> {
    // Todo 10 rejects `{mode,modes}/` as deprecated, so this loader must not
    // resurrect it. The oracle still globs it (`config/agent.ts:32-58`); that
    // divergence is deliberate and is the whole point of the deprecation.
    let dir = tempfile::tempdir()?;
    write(dir.path(), "mode/legacy.md", "---\n---\nA.\n")?;
    write(dir.path(), "modes/older.md", "---\n---\nB.\n")?;
    write(dir.path(), "agent/current.md", "---\n---\nC.\n")?;

    let names: Vec<String> = discover_in_directory(dir.path())?
        .into_iter()
        .map(|found| found.name)
        .collect();
    assert_eq!(names, vec!["current"]);
    Ok(())
}

#[test]
fn dot_directories_are_scanned_because_the_oracle_passes_dot_true() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    write(dir.path(), "agent/.private/secret.md", "---\n---\nA.\n")?;
    let names: Vec<String> = discover_in_directory(dir.path())?
        .into_iter()
        .map(|found| found.name)
        .collect();
    assert_eq!(names, vec![".private/secret"]);
    Ok(())
}

#[test]
fn a_file_that_is_not_markdown_is_ignored() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    write(dir.path(), "agent/readme.txt", "not an agent\n")?;
    write(dir.path(), "agent/real.md", "---\n---\nA.\n")?;
    let names: Vec<String> = discover_in_directory(dir.path())?
        .into_iter()
        .map(|found| found.name)
        .collect();
    assert_eq!(names, vec!["real"]);
    Ok(())
}

#[test]
fn a_missing_agent_directory_is_not_an_error() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    assert!(discover_in_directory(dir.path())?.is_empty());
    Ok(())
}

#[test]
fn discovery_order_is_sorted_so_a_directory_listing_cannot_decide_a_winner()
-> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    for name in ["zebra", "alpha", "middle"] {
        write(dir.path(), &format!("agent/{name}.md"), "---\n---\nA.\n")?;
    }
    let names: Vec<String> = discover_in_directory(dir.path())?
        .into_iter()
        .map(|found| found.name)
        .collect();
    assert_eq!(names, vec!["alpha", "middle", "zebra"]);
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_symlinked_agent_directory_is_followed_without_looping() -> Result<(), Box<dyn Error>> {
    // The oracle scans with `symlink: true`. Following symlinks makes a cycle
    // reachable, so this also proves the walk terminates.
    let dir = tempfile::tempdir()?;
    write(dir.path(), "shared/team.md", "---\n---\nShared.\n")?;
    fs::create_dir_all(dir.path().join("agent"))?;
    std::os::unix::fs::symlink(dir.path().join("shared"), dir.path().join("agent/linked"))?;
    std::os::unix::fs::symlink(dir.path().join("agent"), dir.path().join("agent/loop"))?;

    let names: Vec<String> = discover_in_directory(dir.path())?
        .into_iter()
        .map(|found| found.name)
        .collect();
    assert!(
        names.contains(&"linked/team".to_owned()),
        "the symlinked tree should be scanned, got {names:?}"
    );
    Ok(())
}

#[test]
fn a_file_with_unreadable_frontmatter_is_skipped_not_fatal() -> Result<(), Box<dyn Error>> {
    // `config/agent.ts:19` swallows the parse failure and moves on, so one broken
    // file must not hide every other agent.
    let dir = tempfile::tempdir()?;
    write(
        dir.path(),
        "agent/broken.md",
        "---\n  bad: indentation\n\tmixed\n---\nA.\n",
    )?;
    write(dir.path(), "agent/good.md", "---\n---\nB.\n")?;

    let names: Vec<String> = discover_in_directory(dir.path())?
        .into_iter()
        .map(|found| found.name)
        .collect();
    assert!(names.contains(&"good".to_owned()), "got {names:?}");
    Ok(())
}

#[test]
fn a_frontmatter_name_key_renames_the_agent() -> Result<(), Box<dyn Error>> {
    // Verified against opencode 1.18.12: `agent/original.md` carrying
    // `name: renamed-by-frontmatter` listed under the new name only.
    let dir = tempfile::tempdir()?;
    write(
        dir.path(),
        "agent/original.md",
        "---\nname: renamed-by-frontmatter\nmode: subagent\n---\nA.\n",
    )?;
    let found = discover_in_directory(dir.path())?;
    assert_eq!(found[0].name, "renamed-by-frontmatter");
    Ok(())
}

#[test]
fn the_body_wins_over_a_frontmatter_prompt_key() -> Result<(), Box<dyn Error>> {
    // `{ name, ...md.data, prompt: md.content.trim() }` puts `prompt` last, so the
    // body overwrites any frontmatter `prompt`.
    let dir = tempfile::tempdir()?;
    write(
        dir.path(),
        "agent/x.md",
        "---\nprompt: from frontmatter\n---\nfrom body\n",
    )?;
    let found = discover_in_directory(dir.path())?;
    assert_eq!(found[0].config.prompt.as_deref(), Some("from body"));
    Ok(())
}

#[test]
fn an_empty_body_still_becomes_an_empty_prompt() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    write(dir.path(), "agent/x.md", "---\nmode: all\n---\n")?;
    let found = discover_in_directory(dir.path())?;
    assert_eq!(found[0].config.prompt.as_deref(), Some(""));
    Ok(())
}

#[test]
fn unknown_frontmatter_keys_reach_provider_options() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    write(
        dir.path(),
        "agent/tuned.md",
        "---\nmodel: anthropic/sonnet\nreasoningEffort: high\n---\nA.\n",
    )?;
    let found = discover_in_directory(dir.path())?;
    let options = found[0]
        .config
        .options
        .as_ref()
        .expect("the sweep should have produced options");
    assert_eq!(
        options.get("reasoningEffort"),
        Some(&serde_json::Value::String("high".to_owned()))
    );
    assert!(
        !options.contains_key("model"),
        "a named field must not be swept into options"
    );
    Ok(())
}

#[test]
fn a_later_config_directory_overrides_an_earlier_one() -> Result<(), Box<dyn Error>> {
    // `config/config.ts:460` merges each directory over the accumulated map.
    let global = tempfile::tempdir()?;
    let project = tempfile::tempdir()?;
    write(
        global.path(),
        "agent/shared.md",
        "---\nmode: primary\ndescription: from global\n---\nGlobal body.\n",
    )?;
    write(
        project.path(),
        "agent/shared.md",
        "---\nmode: subagent\n---\nProject body.\n",
    )?;

    let found =
        agent::discover_markdown(&[global.path().to_path_buf(), project.path().to_path_buf()])?;
    let (map, paths) = to_map(found);
    let agents = agent::list(&map, &paths);
    let shared = agents
        .iter()
        .find(|agent| agent.name == "shared")
        .expect("shared exists");
    assert_eq!(shared.mode, AgentMode::Subagent, "the later directory wins");
    assert_eq!(shared.prompt.as_deref(), Some("Project body."));
    Ok(())
}

#[test]
fn read_markdown_agent_names_the_file_it_could_not_read() {
    let missing = Path::new("/nonexistent-root-for-task-13/agent/x.md");
    let error = read_markdown_agent(Path::new("/nonexistent-root-for-task-13"), missing)
        .expect_err("a missing file is an io failure");
    let ConfigError::Io { path, .. } = &error else {
        panic!("expected an io failure, got {error:?}");
    };
    assert_eq!(path, missing);
}

// ---------------------------------------------------------------------------
// Permission rule order, asserted through the real evaluator.
//
// Rule precedence is last-match-wins over the author's key order, so the frontmatter
// parser and the agent merge are part of the permission boundary: a representation
// that sorted keys on the way in would hand the evaluator a policy the file does not
// state. These run the production path — `load_map` over real files, then
// `rules_from_config` and `evaluate` — because key order in a config struct is only
// evidence, and an allow/deny verdict is the behavior.
// ---------------------------------------------------------------------------

/// A project directory with the layered environment `load_map` and `discover_with`
/// read: an isolated home and XDG config root, a `.git` marker so the project is a
/// worktree, and a managed config dir that does not exist so the host's own
/// `/etc/zuno` cannot leak into the fixture.
struct Layered {
    root: tempfile::TempDir,
}

impl Layered {
    fn new() -> Result<Self, Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        for relative in ["home", "xdg-config", "project/.git"] {
            fs::create_dir_all(root.path().join(relative))?;
        }
        Ok(Self { root })
    }

    fn project(&self) -> std::path::PathBuf {
        self.root.path().join("project")
    }

    fn env(&self) -> Env {
        let path = |relative: &str| self.root.path().join(relative).display().to_string();
        Env::from_pairs([
            ("HOME", path("home")),
            ("ZUNO_TEST_HOME", path("home")),
            ("XDG_CONFIG_HOME", path("xdg-config")),
            ("ZUNO_TEST_MANAGED_CONFIG_DIR", path("managed-absent")),
        ])
    }

    fn write(&self, relative: &str, contents: &str) -> Result<(), Box<dyn Error>> {
        write(self.root.path(), relative, contents)
    }

    fn load_agent_policy(&self, name: &str) -> Result<PermissionConfig, Box<dyn Error>> {
        let loaded = load_map(&self.project(), Some(&self.project()), &self.env())?;
        Ok(loaded
            .agents
            .get(name)
            .and_then(|config| config.permission.clone())
            .ok_or_else(|| format!("agent {name} has no permission policy"))?)
    }

    fn discover_agent_policy(&self, name: &str) -> Result<PermissionConfig, Box<dyn Error>> {
        let options = DiscoveryOptions::new(self.project(), Some(self.project()), self.env())
            .with_default_username("unknown");
        let config = discover_with(&options)?;
        Ok(config
            .agent
            .as_ref()
            .and_then(|agents| agents.get(name))
            .and_then(|config| config.permission.clone())
            .ok_or_else(|| format!("agent {name} has no permission policy"))?)
    }
}

/// `(permission, pattern, action)` for every flattened rule, in evaluation order.
fn shape(policy: &PermissionConfig) -> Vec<(String, String, PermissionAction)> {
    rules_from_config(policy)
        .into_iter()
        .map(|rule| (rule.permission, rule.pattern, rule.action))
        .collect()
}

/// The verdict for `tool` on `resource` under `policy`, through the real evaluator.
fn verdict(policy: &PermissionConfig, tool: &str, resource: &str) -> PermissionAction {
    evaluate(permission_key(tool), resource, &rules_from_config(policy))
}

/// The directory `$HOME` expanded to in `policy`'s `.ssh` rule.
///
/// `rules_from_config` expands `$HOME` with the process's own home directory, not
/// the fixture's, so the resource under test is derived from the expanded rule
/// rather than from an assumption about the host.
fn expanded_home(policy: &PermissionConfig) -> String {
    rules_from_config(policy)
        .iter()
        .find_map(|rule| rule.pattern.strip_suffix("/.ssh/*").map(str::to_owned))
        .expect("the policy carries the .ssh rule")
}

/// The audit's failure scenario, end to end: the exact shape
/// `docs/guide/permissions.md` tells users to write. `$HOME/.ssh/*` (0x24) sorts
/// before `*` (0x2A), so an alphabetized copy of this block evaluates deny first and
/// allow last, and the private key is readable without a prompt.
#[test]
fn frontmatter_permission_rules_reach_the_evaluator_in_the_authors_order()
-> Result<(), Box<dyn Error>> {
    let layered = Layered::new()?;
    layered.write(
        "project/.zuno/agents/reviewer.md",
        r#"---
description: Reviews a diff and reports findings without editing
mode: subagent
permission:
  rules:
    read:
      "*": allow
      "$HOME/.ssh/*": deny
---
Review with care.
"#,
    )?;

    let policy = layered.load_agent_policy("reviewer")?;
    let home = expanded_home(&policy);
    assert_eq!(
        shape(&policy),
        [
            ("read".to_owned(), "*".to_owned(), PermissionAction::Allow),
            (
                "read".to_owned(),
                format!("{home}/.ssh/*"),
                PermissionAction::Deny
            ),
        ],
        "the flattened rules keep the order the file wrote them in"
    );
    assert_eq!(
        verdict(&policy, "read", &format!("{home}/.ssh/id_rsa")),
        PermissionAction::Deny,
        "the deny is written last, so it is the last match"
    );
    assert_eq!(
        verdict(&policy, "read", "src/main.rs"),
        PermissionAction::Allow,
        "an ordinary file is still governed by the catch-all"
    );

    // The parse step alone reaches the same order, so the merge is not what saved it.
    let parsed = read_markdown_agent(
        &layered.project().join(".zuno"),
        &layered.project().join(".zuno/agents/reviewer.md"),
    )?
    .expect("the agent parses");
    assert_eq!(
        shape(parsed.config.permission.as_ref().expect("policy")),
        shape(&policy)
    );
    Ok(())
}

/// The honest half: honoring the author's order also means honoring it where the
/// alphabetical accident happened to be stricter. `*` sorts before `r` and before
/// `s`, so the sorted copies of both blocks below would evaluate the deny last.
#[test]
fn the_authors_order_wins_where_alphabetical_order_would_have_been_stricter()
-> Result<(), Box<dyn Error>> {
    let layered = Layered::new()?;
    layered.write(
        "project/.zuno/agents/runner.md",
        r#"---
permission:
  rules:
    shell:
      "rm -rf*": deny
      "*": allow
---
Run the build.
"#,
    )?;
    let policy = layered.load_agent_policy("runner")?;
    assert_eq!(
        shape(&policy)
            .iter()
            .map(|(_, pattern, _)| pattern.as_str())
            .collect::<Vec<_>>(),
        ["rm -rf*", "*"]
    );
    assert_eq!(
        verdict(&policy, "shell", "rm -rf /"),
        PermissionAction::Allow,
        "the author wrote the catch-all allow last; sorting would have denied this"
    );

    let layered = Layered::new()?;
    layered.write(
        "project/.zuno/agents/loose.md",
        "---\npermission:\n  rules:\n    shell: deny\n    \"*\": allow\n---\nBody.\n",
    )?;
    let policy = layered.load_agent_policy("loose")?;
    assert_eq!(
        shape(&policy)
            .iter()
            .map(|(permission, _, _)| permission.as_str())
            .collect::<Vec<_>>(),
        ["shell", "*"]
    );
    assert_eq!(
        verdict(&policy, "shell", "ls"),
        PermissionAction::Allow,
        "top-level keys follow the same rule: the catch-all written last wins"
    );
    Ok(())
}

/// A Markdown agent merges over the config-file agent of the same name by exactly
/// the rule two `zuno.json` layers merge by: a key both set keeps the base position
/// and takes the overlay value, a key only the overlay sets is appended. Both
/// pairs below are therefore asserted equal to the same content spelled as a
/// global file plus a project file, verdicts included — the second pair shows an
/// appended overlay catch-all becoming the last match, as it does for file layers.
#[test]
fn a_markdown_overlay_merges_by_the_file_layer_rule() -> Result<(), Box<dyn Error>> {
    let cases: [(&str, &str, &str, &str, PermissionAction); 2] = [
        (
            r#"{"edit":"deny","*":"deny"}"#,
            "edit: allow",
            "edit",
            "notes.txt",
            PermissionAction::Deny,
        ),
        (
            r#"{"shell":"deny"}"#,
            "\"*\": allow",
            "shell",
            "rm -rf /",
            PermissionAction::Allow,
        ),
    ];
    for (base_rules, overlay_rule, tool, resource, expected) in cases {
        let markdown = Layered::new()?;
        markdown.write(
            "project/.zuno/zuno.json",
            &format!(r#"{{"agents":{{"build":{{"permission":{{"rules":{base_rules}}}}}}}}}"#),
        )?;
        markdown.write(
            "project/.zuno/agent/build.md",
            &format!("---\npermission:\n  rules:\n    {overlay_rule}\n---\nBuild.\n"),
        )?;
        let from_markdown = markdown.load_agent_policy("build")?;

        let files = Layered::new()?;
        files.write(
            "xdg-config/zuno/zuno.json",
            &format!(r#"{{"agents":{{"build":{{"permission":{{"rules":{base_rules}}}}}}}}}"#),
        )?;
        let (key, action) = overlay_rule
            .split_once(": ")
            .expect("fixture rule is `key: action`");
        files.write(
            "project/.zuno/zuno.json",
            &format!(
                r#"{{"agents":{{"build":{{"permission":{{"rules":{{{}:"{action}"}}}}}}}}}}"#,
                if key.starts_with('"') {
                    key.to_owned()
                } else {
                    format!("\"{key}\"")
                }
            ),
        )?;
        let from_files = files.discover_agent_policy("build")?;

        assert_eq!(
            shape(&from_markdown),
            shape(&from_files),
            "{base_rules} + {overlay_rule}: the Markdown overlay and a project file merge identically"
        );
        assert_eq!(
            verdict(&from_markdown, tool, resource),
            expected,
            "{base_rules} + {overlay_rule}: rules were {:?}",
            shape(&from_markdown)
        );
        assert_eq!(verdict(&from_files, tool, resource), expected);
    }
    Ok(())
}

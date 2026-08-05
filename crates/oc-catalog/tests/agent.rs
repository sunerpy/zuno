//! On-disk agent discovery: the name rule, the body-to-prompt rule, and the
//! deprecated-key rejection, exercised against real directory trees.
//!
//! The unit tests in `src/agent.rs` cover the pure fold. These cover what only a
//! filesystem can: the `{agent,agents}` roots, nested paths, dot-directories,
//! symlinks, and the two QA scenarios named in this task's plan entry.

use oc_catalog::agent::{self, AgentMode, AgentSource, discover_in_directory, read_markdown_agent};
use oc_config::schema::agent::AgentConfig;
use oc_config::schema::ordered::OrderedMap;
use oc_error::ConfigError;
use std::error::Error;
use std::fs;
use std::path::Path;

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
fn a_markdown_agent_with_tools_is_rejected_too() -> Result<(), Box<dyn Error>> {
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
        rendered.contains("permission"),
        "the message must point at `permission`, got: {rendered}"
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

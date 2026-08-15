//! Agent discovery over a layered on-disk config tree.
//!
//! This file is what survived the removal of the opencode differential suite. The
//! seven comparative tests there asked *"does `zuno agent list` print what
//! `opencode agent list` prints"*, a question Zuno no longer answers to. The one
//! test kept here never asked it: it drives [`agent::load`] against a fixture and
//! pins what Zuno itself must produce.
//!
//! It is not a weak assertion. The fixture places three Markdown agents at nested
//! paths across two different config directories and adds two config-file entries
//! that override built-ins, so the assertions below cover the whole path-derived
//! name rule, override precedence over a built-in, the `mode: "all"` default for
//! frontmatter that omits `mode`, and body-to-prompt trimming.

use oc_catalog::agent;
use oc_paths::Env;
use oc_testkit::ConfigFixture;
use std::error::Error;
use std::fs;

/// Write a file under `$XDG_CONFIG_HOME/opencode/`, one of the global config
/// directories this crate scans for `{agent,agents}/**/*.md`.
fn global_asset(
    fixture: &ConfigFixture,
    relative: &str,
    contents: &str,
) -> Result<(), Box<dyn Error>> {
    for directory in ["opencode", "zuno"] {
        let path = fixture.env().xdg_config().join(directory).join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)?;
    }
    Ok(())
}

/// Write a file under `<project>/.opencode/`, the nearest project config
/// directory.
fn project_asset(
    fixture: &ConfigFixture,
    relative: &str,
    contents: &str,
) -> Result<(), Box<dyn Error>> {
    for directory in [".opencode", ".zuno"] {
        let path = fixture.env().project().join(directory).join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)?;
    }
    Ok(())
}

/// A fixture with three Markdown agents at nested paths, in two different config
/// directories, plus two config entries that override built-ins.
fn fixture() -> Result<ConfigFixture, Box<dyn Error>> {
    let fixture = ConfigFixture::new()?.mark_worktree_root("")?.global(
        r#"{
  "model": "differential/model",
  "agent": {
    "plan": { "description": "overridden plan", "mode": "all" },
    "title": { "description": "overridden title", "temperature": 0.9 }
  }
}"#,
    )?;

    global_asset(
        &fixture,
        "agent/review/security.md",
        "---\ndescription: Reviews code for security problems\nmode: subagent\n---\n\
         You are a security reviewer.\n\nLook for injection flaws.\n",
    )?;
    global_asset(
        &fixture,
        "agent/review/performance.md",
        "---\ndescription: Reviews code for slow paths\n---\nFind the hot loop.\n",
    )?;
    project_asset(
        &fixture,
        "agent/deep/nested/thing.md",
        "---\ndescription: three levels down\nmode: primary\n---\nNested body.\n",
    )?;

    Ok(fixture)
}

#[test]
fn nested_names_and_built_in_overrides_resolve() -> Result<(), Box<dyn Error>> {
    // A loader that returned a bare built-in list would satisfy a weaker test, so
    // this pins every field the fixture was built to exercise.
    let fixture = fixture()?;
    let directory = fixture.env().working_dir().to_path_buf();
    let project = fixture.env().project().to_path_buf();
    let env = Env::from_pairs(fixture.env().env_vars());

    let agents = agent::load(&directory, Some(project.as_path()), &env)?;
    let names: Vec<&str> = agents.iter().map(|agent| agent.name.as_str()).collect();

    for expected in ["review/security", "review/performance", "deep/nested/thing"] {
        assert!(
            names.contains(&expected),
            "the fixture must produce the nested agent {expected}, got {names:?}"
        );
    }

    let plan = agents
        .iter()
        .find(|agent| agent.name == "plan")
        .expect("plan exists");
    assert_eq!(plan.mode, agent::AgentMode::All, "plan must be overridden");
    assert!(
        plan.source.is_native(),
        "an overridden built-in stays native"
    );

    let title = agents
        .iter()
        .find(|agent| agent.name == "title")
        .expect("title exists");
    assert_eq!(title.temperature, Some(0.9), "title must be overridden");

    let performance = agents
        .iter()
        .find(|agent| agent.name == "review/performance")
        .expect("review/performance exists");
    assert_eq!(
        performance.mode,
        agent::AgentMode::All,
        "an agent whose frontmatter omits `mode` defaults to all"
    );
    assert_eq!(
        performance.prompt.as_deref(),
        Some("Find the hot loop."),
        "the trimmed body is the prompt"
    );
    Ok(())
}

//! Differential: this crate's agent list against the real `opencode`'s.
//!
//! # What is compared, and why not more
//!
//! The plan entry for this task asks for parity with
//! `opencode agent list --format json`. **That flag does not exist.** Run against
//! opencode 1.18.12 it exits 1 with a yargs usage error and prints nothing to
//! stdout, because `cli/cmd/agent.ts:235-257` declares no options at all. The real
//! command is `opencode agent list`, and it prints, per agent:
//!
//! ```text
//! <name> (<mode>)
//!   <the permission ruleset as indented JSON>
//! ```
//!
//! So this differential compares the `name (mode)` header of every agent, in the
//! order the oracle prints them. That is the whole of the output this task owns:
//! the permission ruleset is a resolved ruleset, and resolving one needs the
//! runtime default set, the `Truncate.GLOB` whitelist, the discovered skill and
//! reference directories, and a worktree-relative rewrite — all of which belong to
//! the permission tasks. Comparing a ruleset this crate does not compute would
//! either fail for reasons outside this task or force a normalizer wide enough to
//! hide a real difference.
//!
//! The headers are not a weak assertion. They carry every field this task is
//! responsible for: which agents exist, what each is called (so the whole
//! path-derived name rule), whether a user definition overrode a built-in or
//! created a new one, the `mode: "all"` default, and the native-first sort order.

use oc_catalog::agent;
use oc_paths::Env;
use oc_testkit::{ConfigFixture, Normalizer, Oracle, diff_normalized, pinned_oracle_or_skip};
use std::error::Error;
use std::fs;
use std::path::Path;

/// Divergences accepted on purpose, each with the reason it is not a defect.
///
/// Empty. Todo 12's config differential carries exactly one entry; this one needs
/// none, because the header line has no key-order or formatting freedom.
const INTENTIONAL_DIVERGENCES: &[(&str, &str)] = &[];

fn oracle_or_skip(test: &str, fixture: ConfigFixture) -> Option<Oracle> {
    let program = pinned_oracle_or_skip(
        test,
        "no agent listing was compared against the pinned opencode release",
    )?;
    Some(
        Oracle::at_binary(program)
            .expect("the centrally screened oracle must still be runnable")
            .with_env(fixture.into_env()),
    )
}

/// Write a file under `$XDG_CONFIG_HOME/opencode/`, which is the global config
/// directory both this crate and the oracle scan for `{agent,agents}/**/*.md`.
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

/// The `name (mode)` headers the oracle printed, in its own order.
///
/// The permission JSON that follows each header is indented, except for its
/// closing `]`, which sits at column 0 — so a header is recognized by the trailing
/// ` (<mode>)` rather than by indentation alone.
fn oracle_headers(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter(|line| !line.starts_with(char::is_whitespace))
        .filter(|line| {
            ["(subagent)", "(primary)", "(all)"]
                .iter()
                .any(|suffix| line.ends_with(suffix))
        })
        .map(str::to_owned)
        .collect()
}

fn rendered(headers: &[String]) -> String {
    let mut out = String::new();
    for header in headers {
        out.push_str(header);
        out.push('\n');
    }
    out
}

/// How many oracle runs are attempted before an unstable output is a failure.
///
/// Generous because the oracle's truncation gets much more likely as host load
/// rises, and this suite is run concurrently with three sibling crates' suites.
const STABILITY_ATTEMPTS: usize = 8;

/// Run `opencode agent list` until two consecutive well-formed runs agree, and
/// return the headers they agreed on.
///
/// # The oracle bug this works around
///
/// `cli/cmd/agent.ts:248-251` emits each agent with `process.stdout.write` and then
/// lets the process exit. On a pipe those writes are asynchronous, so under load the
/// runtime can exit before the tail is flushed. Three distinct losses were observed
/// on opencode 1.18.12 with four copies of this suite running concurrently:
///
/// * five of the seven built-ins, dropping `summary` and `title`;
/// * all seven built-ins but not the user's `collide`;
/// * a listing cut off in the middle of `build`'s permission JSON.
///
/// Every loss was a suffix, which is the signature of a dropped flush rather than of
/// a decision the oracle made. The volume matters: each agent is followed by its
/// whole permission ruleset as pretty-printed JSON, so a listing is kilobytes of
/// output and there is plenty to lose.
///
/// # Why the criterion is agreement plus well-formedness
///
/// The obvious check — "every agent I expect must be present" — cannot tell a
/// dropped flush from the oracle genuinely not defining an agent, so it would report
/// a real behavioural difference as a truncation and hide it. Agreement between two
/// consecutive runs assumes nothing about which agents should exist: the oracle is
/// deterministic, so two runs differ only when I/O interfered.
///
/// Agreement alone is not enough, and that is not hypothetical — a mid-block
/// truncation was observed to repeat identically, presumably because the loss point
/// follows the pipe's capacity rather than a coin flip. So a run also has to *look*
/// complete, which for this command means ending with the `]` that closes the last
/// agent's ruleset. Runs failing that are discarded rather than compared.
///
/// If the output never stabilizes the test fails and says so, which is the safe
/// direction — a flaky oracle is reported, never silently tolerated. A normalizer is
/// deliberately not used: one loose enough to absorb missing agents would also
/// absorb this crate failing to define them, which is the exact failure this task
/// exists to prevent.
fn oracle_agent_headers(oracle: &Oracle) -> Result<Vec<String>, Box<dyn Error>> {
    let mut previous: Option<String> = None;
    let mut discarded = 0usize;
    for attempt in 1..=STABILITY_ATTEMPTS {
        let outcome = oracle.run(["agent", "list"])?;
        assert!(
            outcome.is_success(),
            "the oracle failed to list agents on attempt {attempt}:\n{}",
            outcome.render()
        );
        let stdout = outcome.stdout;

        if !is_well_formed(&stdout) {
            discarded += 1;
            eprintln!(
                "oracle listing was truncated on attempt {attempt} ({} bytes, \
                 does not end with the closing bracket); discarding",
                stdout.len()
            );
            previous = None;
            continue;
        }

        match previous {
            Some(earlier) if earlier == stdout => return Ok(oracle_headers(&stdout)),
            Some(earlier) => {
                eprintln!(
                    "oracle output was unstable on attempt {attempt} ({} vs {} bytes); retrying",
                    earlier.len(),
                    stdout.len()
                );
                previous = Some(stdout);
            }
            None => previous = Some(stdout),
        }
    }
    Err(format!(
        "the oracle never produced the same well-formed agent listing twice in \
         {STABILITY_ATTEMPTS} attempts ({discarded} were truncated)"
    )
    .into())
}

/// Whether the listing ends where a complete one does.
///
/// Each agent prints its header, then its permission ruleset as indented JSON whose
/// closing `]` sits at column 0. So a listing that did not lose its tail ends with
/// that bracket.
fn is_well_formed(stdout: &str) -> bool {
    stdout
        .lines()
        .rfind(|line| !line.trim().is_empty())
        .is_some_and(|line| line == "]")
}

/// A fixture with three Markdown agents at nested paths, in two different config
/// directories, plus two config entries that override built-ins.
///
/// This is the exact shape this task's acceptance criterion names.
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
fn the_agent_list_matches_real_opencode() -> Result<(), Box<dyn Error>> {
    for (case, reason) in INTENTIONAL_DIVERGENCES {
        assert!(
            !reason.trim().is_empty(),
            "intentional divergence {case} needs a one-line reason"
        );
    }

    let fixture = fixture()?;
    let directory = fixture.env().working_dir().to_path_buf();
    let project = fixture.env().project().to_path_buf();
    let env = Env::from_pairs(fixture.env().env_vars());

    let Some(oracle) = oracle_or_skip("the full oc-catalog agent differential", fixture) else {
        return Ok(());
    };
    let oracle_headers = oracle_agent_headers(&oracle)?;
    assert!(
        oracle_headers.len() >= 10,
        "expected the seven built-ins plus three Markdown agents, got {oracle_headers:?}"
    );

    let rust = agent::load(&directory, Some(project.as_path()), &env)?;
    let rust_headers: Vec<String> = rust.iter().map(agent::Agent::header).collect();

    let report = diff_normalized(
        oracle.provenance().label(),
        &rendered(&oracle_headers),
        "oc-catalog agent list".to_owned(),
        &rendered(&rust_headers),
        &Normalizer::none(),
    );
    println!(
        "oracle: {} ({})\n{}",
        oracle.reported_version(),
        oracle.version_gap().describe(),
        report.render()
    );
    report.assert_identical();
    Ok(())
}

#[test]
fn the_fixture_really_exercises_nested_names_and_built_in_overrides() -> Result<(), Box<dyn Error>>
{
    // A differential that passes because both sides produced a bare built-in list
    // would prove nothing, so this pins what the fixture must contain.
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

#[test]
fn the_agent_list_matches_real_opencode_with_no_user_agents_at_all() -> Result<(), Box<dyn Error>> {
    // The baseline: the seven built-ins, their modes, and their sort order, with
    // nothing of the user's in the way.
    let fixture = ConfigFixture::new()?
        .mark_worktree_root("")?
        .global(r#"{"model":"differential/model"}"#)?;
    let directory = fixture.env().working_dir().to_path_buf();
    let project = fixture.env().project().to_path_buf();
    let env = Env::from_pairs(fixture.env().env_vars());

    let Some(oracle) = oracle_or_skip("the built-in-only oc-catalog agent differential", fixture)
    else {
        return Ok(());
    };
    let oracle_lines = oracle_agent_headers(&oracle)?;

    let rust = agent::load(&directory, Some(project.as_path()), &env)?;
    let report = diff_normalized(
        oracle.provenance().label(),
        &rendered(&oracle_lines),
        "oc-catalog agent list, no user agents".to_owned(),
        &rendered(&rust.iter().map(agent::Agent::header).collect::<Vec<_>>()),
        &Normalizer::none(),
    );
    println!("oracle: {}\n{}", oracle.reported_version(), report.render());
    report.assert_identical();
    Ok(())
}

#[test]
fn a_markdown_agent_beats_a_config_file_entry_of_the_same_name() -> Result<(), Box<dyn Error>> {
    // `config/config.ts:460` merges the Markdown layer over the config files, so a
    // same-named Markdown definition wins. Confirmed against the binary here
    // rather than asserted from source.
    let fixture = ConfigFixture::new()?
        .mark_worktree_root("")?
        .global(r#"{"model":"differential/model","agent":{"collide":{"mode":"primary"}}}"#)?;
    global_asset(
        &fixture,
        "agent/collide.md",
        "---\nmode: subagent\n---\nFrom markdown.\n",
    )?;

    let directory = fixture.env().working_dir().to_path_buf();
    let project = fixture.env().project().to_path_buf();
    let env = Env::from_pairs(fixture.env().env_vars());

    let Some(oracle) = oracle_or_skip("the Markdown-over-config agent differential", fixture)
    else {
        return Ok(());
    };
    let oracle_lines = oracle_agent_headers(&oracle)?;
    assert!(
        oracle_lines.contains(&"collide (subagent)".to_owned()),
        "the oracle should report the Markdown mode, got {oracle_lines:?}"
    );

    let rust = agent::load(&directory, Some(project.as_path()), &env)?;
    let report = diff_normalized(
        oracle.provenance().label(),
        &rendered(&oracle_lines),
        "oc-catalog agent list, markdown over config".to_owned(),
        &rendered(&rust.iter().map(agent::Agent::header).collect::<Vec<_>>()),
        &Normalizer::none(),
    );
    println!("oracle: {}\n{}", oracle.reported_version(), report.render());
    report.assert_identical();
    Ok(())
}

#[test]
fn opencode_config_content_beats_a_markdown_agent() -> Result<(), Box<dyn Error>> {
    // The other boundary of the layer order: `OPENCODE_CONFIG_CONTENT` merges
    // after the Markdown layer (`config/config.ts:467-475`), so it wins. This is
    // the case a naive "merge Markdown last" implementation gets wrong, and it is
    // silent when it does.
    let fixture = ConfigFixture::new()?
        .mark_worktree_root("")?
        .global(r#"{"model":"differential/model"}"#)?;
    global_asset(
        &fixture,
        "agent/contentwin.md",
        "---\nmode: subagent\n---\nFrom markdown.\n",
    )?;
    let fixture = fixture.env_config_content(r#"{"agent":{"contentwin":{"mode":"all"}}}"#);

    let directory = fixture.env().working_dir().to_path_buf();
    let project = fixture.env().project().to_path_buf();
    let env = Env::from_pairs(fixture.env().env_vars());

    let Some(oracle) = oracle_or_skip("the config-content-over-Markdown differential", fixture)
    else {
        return Ok(());
    };
    let oracle_lines = oracle_agent_headers(&oracle)?;
    assert!(
        oracle_lines.contains(&"contentwin (all)".to_owned()),
        "the env layer should win over Markdown, got {oracle_lines:?}"
    );

    let rust = agent::load(&directory, Some(project.as_path()), &env)?;
    let report = diff_normalized(
        oracle.provenance().label(),
        &rendered(&oracle_lines),
        "oc-catalog agent list, env content over markdown".to_owned(),
        &rendered(&rust.iter().map(agent::Agent::header).collect::<Vec<_>>()),
        &Normalizer::none(),
    );
    println!("oracle: {}\n{}", oracle.reported_version(), report.render());
    report.assert_identical();
    Ok(())
}

#[test]
fn the_oracle_has_no_format_json_flag() -> Result<(), Box<dyn Error>> {
    // This task's plan entry asks for parity with `agent list --format json`. The
    // flag does not exist, and this test is what keeps that correction from being
    // quietly forgotten: if a future opencode adds the flag, this fails and the
    // differential above should be upgraded to compare the richer output.
    let fixture = ConfigFixture::new()?
        .mark_worktree_root("")?
        .global(r#"{"model":"differential/model"}"#)?;
    let Some(oracle) = oracle_or_skip("the agent-list format-flag differential", fixture) else {
        return Ok(());
    };
    let outcome = oracle.run(["agent", "list", "--format", "json"])?;
    assert!(
        !outcome.is_success(),
        "opencode {} unexpectedly accepted --format json; its stdout was:\n{}",
        oracle.reported_version(),
        outcome.stdout
    );
    assert!(
        outcome.stdout.trim().is_empty(),
        "the rejected command should print nothing to stdout, got:\n{}",
        outcome.stdout
    );
    Ok(())
}

#[test]
fn every_config_directory_is_scanned_for_markdown_agents() -> Result<(), Box<dyn Error>> {
    // The plan says "every config dir". This puts one agent in the global config
    // directory and one in the project `.opencode`, and requires both to appear.
    let fixture = ConfigFixture::new()?
        .mark_worktree_root("")?
        .global(r#"{"model":"differential/model"}"#)?;
    global_asset(&fixture, "agents/from-global.md", "---\n---\nGlobal.\n")?;
    project_asset(&fixture, "agent/from-project.md", "---\n---\nProject.\n")?;

    let directory = fixture.env().working_dir().to_path_buf();
    let project = fixture.env().project().to_path_buf();
    let env = Env::from_pairs(fixture.env().env_vars());

    let Some(oracle) = oracle_or_skip("the all-config-directories agent differential", fixture)
    else {
        return Ok(());
    };
    let oracle_lines = oracle_agent_headers(&oracle)?;
    for expected in ["from-global (all)", "from-project (all)"] {
        assert!(
            oracle_lines.contains(&expected.to_owned()),
            "the oracle should have found {expected}, got {oracle_lines:?}"
        );
    }

    let rust = agent::load(&directory, Some(project.as_path()), &env)?;
    let report = diff_normalized(
        oracle.provenance().label(),
        &rendered(&oracle_lines),
        "oc-catalog agent list, both config directories".to_owned(),
        &rendered(&rust.iter().map(agent::Agent::header).collect::<Vec<_>>()),
        &Normalizer::none(),
    );
    println!("oracle: {}\n{}", oracle.reported_version(), report.render());
    report.assert_identical();
    Ok(())
}

#[test]
fn the_working_directory_of_the_fixture_is_inside_the_project() -> Result<(), Box<dyn Error>> {
    // A guard on the harness rather than on the subject: if the fixture's working
    // directory ever escaped the project, the project `.opencode` agents would
    // silently stop being discovered and the differentials above would still pass.
    let fixture = fixture()?;
    let working = fixture.env().working_dir().to_path_buf();
    let project = fixture.env().project().to_path_buf();
    assert!(
        working.starts_with(&project) || working == project,
        "working dir {} escaped project {}",
        working.display(),
        project.display()
    );
    assert!(Path::new(&project).join(".opencode").exists());
    assert!(Path::new(&project).join(".zuno").exists());
    Ok(())
}

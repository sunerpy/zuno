//! One isolated TypeScript TUI workload run.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::{Result, TestkitError};
use crate::{CassettePlayer, DbChoice, MockProvider, Scenario, ScriptedEnv};

use super::baseline::{RssSample, RunMeasurement, WorkloadName};
use super::database::RealDatabaseSnapshot;
use super::fixtures::{create_watcher_tree, provider_config, write_memory_driver_tool};
use super::process_tree::{find_oracle_descendant, sample};
use super::runner::{SAMPLE_INTERVAL, WARM_UP};

const RESPONSES_PER_TURN: usize = 2;
const TITLE_REQUESTS: usize = 1;

pub(crate) const fn completed_tool_turns(captured_requests: usize) -> usize {
    captured_requests.saturating_sub(TITLE_REQUESTS) / RESPONSES_PER_TURN
}

pub(crate) async fn measure_one(
    oracle_program: &Path,
    workload: WorkloadName,
    repetition: usize,
    turns: usize,
    duration: Duration,
    database: Option<&RealDatabaseSnapshot>,
) -> Result<RunMeasurement> {
    let mut env = ScriptedEnv::new()?;
    if workload == WorkloadName::WSoak {
        create_watcher_tree(env.project())?;
    }
    write_memory_driver_tool(&env, workload == WorkloadName::WSoak)?;
    let session = database.map(|snapshot| snapshot.session.id.clone());
    if let Some(snapshot) = database {
        let path = snapshot.writable_clone(&env.root().join(format!("real-{repetition}.db")))?;
        env = env.with_db(DbChoice::Absolute(path));
    }

    let title_cassette_name = "openai-chat/streams-text";
    let title_player = CassettePlayer::from_oracle(title_cassette_name)?;
    let cassette_name = "openai-chat/drives-a-tool-loop-end-to-end";
    let player = CassettePlayer::from_oracle(cassette_name)?;
    let mut scenario = Scenario::new("baseline-tool-loop")
        .from_cassette(title_cassette_name, title_player.cassette())?;
    for _ in 0..turns {
        scenario = scenario.from_cassette(cassette_name, player.cassette())?;
    }
    let provider = MockProvider::start(vec![scenario]).await?;
    let mut variables = env.env_vars();
    variables.extend([
        ("TERM".to_owned(), "xterm-256color".to_owned()),
        ("COLORTERM".to_owned(), "truecolor".to_owned()),
        ("OPENCODE_PURE".to_owned(), "1".to_owned()),
        ("OPENCODE_AUTH_CONTENT".to_owned(), "{}".to_owned()),
        (
            "OPENCODE_CONFIG_CONTENT".to_owned(),
            provider_config(provider.base_url()),
        ),
    ]);
    let script = which::which("script").map_err(|_| TestkitError::HelperCommandNotFound {
        command: "script",
        remedy: "install util-linux; the TUI must run in a real PTY",
    })?;
    let args = vec![
        "-qefc".to_owned(),
        oracle_command(oracle_program, session.as_deref()),
        "/dev/null".to_owned(),
    ];
    let mut child = spawn_script(&script, &args, env.working_dir(), &variables)?;
    let started = Instant::now();
    let oracle_pid = wait_for_oracle_pid(&mut child, workload, started).await?;
    let result = sample_run(
        &mut child, oracle_pid, workload, turns, duration, started, &provider,
    )
    .await;
    terminate_tree(oracle_pid, &mut child);
    provider.shutdown().await;
    let samples = result?;
    let peak_rss_kib = samples
        .iter()
        .filter(|sample| sample.elapsed_ms >= WARM_UP.as_millis() as u64)
        .map(|sample| sample.total_rss_kib)
        .max()
        .ok_or(TestkitError::BaselineRunFailed {
            workload: workload_label(workload),
            detail: "no RSS samples remained after the frozen 90-second warm-up".to_owned(),
        })?;
    Ok(RunMeasurement {
        repetition,
        peak_rss_kib,
        samples,
    })
}

async fn sample_run(
    child: &mut Child,
    oracle_pid: u32,
    workload: WorkloadName,
    turns: usize,
    duration: Duration,
    started: Instant,
    provider: &MockProvider,
) -> Result<Vec<RssSample>> {
    let mut samples = Vec::new();
    let mut next_sample = Duration::ZERO;
    let mut submitted_turns = 1_usize;
    while started.elapsed() < duration {
        fail_if_exited(child, workload)?;
        if started.elapsed() >= next_sample {
            samples.push(sample(oracle_pid, started)?);
            next_sample += SAMPLE_INTERVAL;
        }
        let completed_turns = completed_tool_turns(provider.captured_count().await);
        if submitted_turns < turns && completed_turns >= submitted_turns {
            submit_next_turn(child, workload)?;
            submitted_turns += 1;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let captured_requests = provider.captured_count().await;
    let completed_turns = completed_tool_turns(captured_requests);
    if completed_turns < turns {
        return Err(TestkitError::BaselineRunFailed {
            workload: workload_label(workload),
            detail: format!(
                "only {completed_turns} of {turns} cassette-backed turns completed; \
                 captured {captured_requests} provider request(s)"
            ),
        });
    }
    Ok(samples)
}

fn submit_next_turn(child: &mut Child, workload: WorkloadName) -> Result<()> {
    let stdin = child
        .stdin
        .as_mut()
        .ok_or(TestkitError::BaselineRunFailed {
            workload: workload_label(workload),
            detail: "PTY launcher did not expose stdin for subsequent soak turns".to_owned(),
        })?;
    stdin
        .write_all(b"Use get_weather for Paris.\r")
        .map_err(|source| TestkitError::io("write W-soak prompt to PTY", "script stdin", source))?;
    stdin
        .flush()
        .map_err(|source| TestkitError::io("flush W-soak prompt to PTY", "script stdin", source))
}

fn spawn_script(
    program: &Path,
    args: &[String],
    cwd: &Path,
    variables: &BTreeMap<String, String>,
) -> Result<Child> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in variables {
        command.env(key, value);
    }
    command.spawn().map_err(|source| TestkitError::Spawn {
        program: program.to_path_buf(),
        args: args.to_vec(),
        source,
    })
}

async fn wait_for_oracle_pid(
    child: &mut Child,
    workload: WorkloadName,
    started: Instant,
) -> Result<u32> {
    while started.elapsed() < Duration::from_secs(30) {
        if let Some(pid) = find_oracle_descendant(child.id())? {
            return Ok(pid);
        }
        fail_if_exited(child, workload)?;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(TestkitError::BaselineRunFailed {
        workload: workload_label(workload),
        detail: "no opencode process appeared under the PTY launcher within 30s".to_owned(),
    })
}

fn fail_if_exited(child: &mut Child, workload: WorkloadName) -> Result<()> {
    if let Some(status) = child.try_wait().map_err(|source| TestkitError::Spawn {
        program: PathBuf::from("script"),
        args: Vec::new(),
        source,
    })? {
        return Err(TestkitError::BaselineRunFailed {
            workload: workload_label(workload),
            detail: format!("oracle TUI exited early with {status}"),
        });
    }
    Ok(())
}

pub(crate) fn oracle_command(program: &Path, session: Option<&str>) -> String {
    let mut args = vec![
        shell_quote(&program.to_string_lossy()),
        "--pure".to_owned(),
        "--prompt".to_owned(),
        shell_quote("Use get_weather for Paris."),
        "--model".to_owned(),
        "test/test-model".to_owned(),
        "--auto".to_owned(),
    ];
    if let Some(session) = session {
        args.extend(["--session".to_owned(), shell_quote(session)]);
    }
    args.join(" ")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn terminate_tree(oracle_pid: u32, launcher: &mut Child) {
    let _ = Command::new("kill")
        .args(["-TERM", &oracle_pid.to_string()])
        .status();
    let _ = launcher.kill();
    let _ = launcher.wait();
}

const fn workload_label(workload: WorkloadName) -> &'static str {
    match workload {
        WorkloadName::WIdle => "W-idle",
        WorkloadName::WReal => "W-real",
        WorkloadName::WSoak => "W-soak",
    }
}

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
use super::process_tree::{find_named_descendant, sample};
use super::runner::{SAMPLE_INTERVAL, WARM_UP, warm_up_discard};

const RESPONSES_PER_TURN: usize = 2;
const PRELUDE_REQUESTS: usize = 1;

/// How the oracle reaches its first turn, which differs between a new session
/// and one restored from the user's database.
///
/// Both open with exactly one tool-free text request, so the cassette prelude is
/// unconditional — but they are not the same request, and the difference was only
/// visible by watching real provider traffic from the 1.18.12 binary:
///
/// - A **new** session's prelude generates the session title, and `--prompt` is
///   submitted for it automatically.
/// - A **restored** session's prelude is a compaction summary, because W-real
///   selects the largest session and it overflows the model's context window.
///   Answering that request from the tool-loop cassette fails the entire turn
///   with `Tool call not allowed while generating summary`. `--prompt` is also
///   discarded in favour of the session's saved draft input, so the turn starts
///   only once text is typed into the PTY, after hydration settles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TurnPlan {
    pub(super) submit_first_turn: bool,
}

impl TurnPlan {
    pub(crate) const fn for_session(resumed: bool) -> Self {
        Self {
            submit_first_turn: resumed,
        }
    }
}

pub(crate) const fn completed_tool_turns(captured_requests: usize) -> usize {
    captured_requests.saturating_sub(PRELUDE_REQUESTS) / RESPONSES_PER_TURN
}

/// Peak total-tree RSS over the samples a workload's warm-up rule retains.
///
/// Shared with the artifact test so a stored peak cannot drift from this rule.
pub(crate) fn peak_after_warm_up(samples: &[RssSample], workload: WorkloadName) -> Option<u64> {
    let discard_before_ms = warm_up_discard(workload).as_millis() as u64;
    samples
        .iter()
        .filter(|sample| sample.elapsed_ms >= discard_before_ms)
        .map(|sample| sample.total_rss_kib)
        .max()
}

#[derive(Debug, Clone, Copy)]
struct RunShape {
    workload: WorkloadName,
    plan: TurnPlan,
    turns: usize,
    duration: Duration,
}

pub(crate) async fn measure_one(
    oracle_program: &Path,
    process_name: &str,
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

    let plan = TurnPlan::for_session(database.is_some());
    let prelude_cassette_name = "openai-chat/streams-text";
    let prelude_player = CassettePlayer::from_oracle(prelude_cassette_name)?;
    let cassette_name = "openai-chat/drives-a-tool-loop-end-to-end";
    let player = CassettePlayer::from_oracle(cassette_name)?;
    let mut scenario = Scenario::new("baseline-tool-loop")
        .from_cassette(prelude_cassette_name, prelude_player.cassette())?;
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
    let oracle_pid = wait_for_subject_pid(&mut child, process_name, workload, started).await?;
    let shape = RunShape {
        workload,
        plan,
        turns,
        duration,
    };
    let result = sample_run(&mut child, oracle_pid, shape, started, &provider).await;
    terminate_tree(oracle_pid, &mut child);
    provider.shutdown().await;
    let samples = result?;
    let peak_rss_kib =
        peak_after_warm_up(&samples, workload).ok_or_else(|| TestkitError::BaselineRunFailed {
            workload: workload_label(workload),
            detail: format!(
                "no RSS samples remained to take a peak from; this workload discards \
                 its first {}s as warm-up",
                warm_up_discard(workload).as_secs()
            ),
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
    shape: RunShape,
    started: Instant,
    provider: &MockProvider,
) -> Result<Vec<RssSample>> {
    let mut samples = Vec::new();
    let mut next_sample = Duration::ZERO;
    let mut submitted_turns = usize::from(!shape.plan.submit_first_turn);
    while started.elapsed() < shape.duration {
        fail_if_exited(child, shape.workload)?;
        if started.elapsed() >= next_sample {
            samples.push(sample(oracle_pid, started)?);
            next_sample += SAMPLE_INTERVAL;
        }
        let completed_turns = completed_tool_turns(provider.captured_count().await);
        if submitted_turns < shape.turns
            && completed_turns >= submitted_turns
            && hydration_is_settled(submitted_turns, started)
        {
            submit_next_turn(child, shape.workload)?;
            submitted_turns += 1;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let captured_requests = provider.captured_count().await;
    let completed_turns = completed_tool_turns(captured_requests);
    if completed_turns < shape.turns {
        return Err(TestkitError::BaselineRunFailed {
            workload: workload_label(shape.workload),
            detail: format!(
                "only {completed_turns} of {} cassette-backed turns completed; \
                 captured {captured_requests} provider request(s)",
                shape.turns
            ),
        });
    }
    Ok(samples)
}

/// A restored session's first turn waits out the 90-second hydration gate, so the
/// keystrokes reach a TUI that has finished hydrating rather than one still
/// replaying thousands of parts.
pub(crate) fn hydration_is_settled(submitted_turns: usize, started: Instant) -> bool {
    submitted_turns > 0 || started.elapsed() >= WARM_UP
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

async fn wait_for_subject_pid(
    child: &mut Child,
    process_name: &str,
    workload: WorkloadName,
    started: Instant,
) -> Result<u32> {
    while started.elapsed() < Duration::from_secs(30) {
        if let Some(pid) = find_named_descendant(child.id(), process_name)? {
            return Ok(pid);
        }
        fail_if_exited(child, workload)?;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(TestkitError::BaselineRunFailed {
        workload: workload_label(workload),
        detail: format!("no {process_name} process appeared under the PTY launcher within 30s"),
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

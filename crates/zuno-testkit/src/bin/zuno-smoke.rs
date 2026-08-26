//! `zuno-smoke` — the per-artifact smoke test the release pipeline runs.
//!
//! # Why this exists as a binary rather than a `#[test]`
//!
//! `crates/zuno-cli/tests/tool_turn.rs` already drives the real `zuno` binary
//! against a cassette-backed provider, and it is the pattern this file follows.
//! But it finds its subject through `env!("CARGO_BIN_EXE_zuno")`, which
//! resolves at compile time to the binary cargo just built for the host. That is
//! exactly the wrong subject for a release gate: the thing we must prove works is
//! the binary **inside the archive we are about to publish**, after packaging,
//! after transport, on the platform it targets.
//!
//! So the same three checks live here behind `--binary <path>`, and the release
//! workflow points it at the unpacked artifact.
//!
//! # The three checks, and what each one is worth
//!
//! | check | proves |
//! |---|---|
//! | `--version` | the binary loads and its dynamic linkage resolves on this platform |
//! | `--help` | argument parsing is wired and the command tree is registered |
//! | a headless `run` against a cassette | a full turn executes: config discovery, provider dispatch, the tool registry, and the turn loop |
//!
//! Only the third is a real test. The first two are cheap and catch the failure
//! that matters most in a cross-compiled artifact — a binary that cannot start.
//!
//! # No live network
//!
//! The provider is `MockProvider`, always on loopback, replaying a committed
//! recording (see `packaging/smoke/cassettes/PROVENANCE.md`). Credentials are a
//! literal `test-key` that never leaves the machine, models come from a pinned
//! fixture, and `ZUNO_DISABLE_MODELS_FETCH` blocks the one fetch the binary
//! would otherwise attempt.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use zuno_testkit::{MockProvider, MockResponse, Scenario, ScriptedEnv};

/// The committed recording the turn check replays.
const CASSETTE: &str = "openai-chat/drives-a-tool-loop-end-to-end";

/// The tool the recording calls, which this runtime deliberately does not have.
///
/// Its absence is the point: an unknown call still has to produce a tool result
/// that the loop sends back, and that is observable without this runtime owning a
/// tool by that name.
const RECORDED_TOOL: &str = "get_weather";

/// Tools the request must advertise.
///
/// The same four `tool_turn.rs` pins, and for the same reason: `edit`/`write` are
/// model-conditional and `websearch` is provider-conditional, so a longer list
/// would fail for reasons unrelated to whether the registry reached the wire.
const REQUIRED_TOOLS: [&str; 4] = ["shell", "read", "glob", "grep"];

/// Wall-clock budget per subprocess. Everything the subject talks to is loopback
/// or the local filesystem, so exceeding this is a hang, not slowness.
const STEP_TIMEOUT: Duration = Duration::from_secs(60);

/// Exit code for a failed check. Distinct from 1 so a panic and a checked failure
/// are distinguishable in a CI log.
const FAILURE: i32 = 2;

struct Options {
    binary: PathBuf,
    cassette_root: PathBuf,
    models: PathBuf,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let options = match parse_arguments() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("zuno-smoke: {message}\n\n{USAGE}");
            std::process::exit(FAILURE);
        }
    };
    match run(&options).await {
        Ok(()) => println!("zuno-smoke: PASS  {}", options.binary.display()),
        Err(failure) => {
            eprintln!(
                "zuno-smoke: FAIL  {}\n  {failure}",
                options.binary.display()
            );
            std::process::exit(FAILURE);
        }
    }
}

const USAGE: &str = "\
usage: zuno-smoke --binary <path> [--cassette-root <dir>] [--models <file>]

  --binary        the artifact to exercise; required
  --cassette-root directory holding <route>/<name>.json recordings
                  (default: <workspace>/packaging/smoke/cassettes)
  --models        pinned models.dev fixture
                  (default: <workspace>/crates/zuno-llm/tests/fixtures/models-dev-pinned.json)";

fn parse_arguments() -> Result<Options, String> {
    let mut binary = None;
    let mut cassette_root = None;
    let mut models = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(flag) = arguments.next() {
        let mut value = || {
            arguments
                .next()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--binary" => binary = Some(PathBuf::from(value()?)),
            "--cassette-root" => cassette_root = Some(PathBuf::from(value()?)),
            "--models" => models = Some(PathBuf::from(value()?)),
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unrecognised argument `{other}`")),
        }
    }
    let binary = binary.ok_or("--binary is required")?;
    if !binary.is_file() {
        return Err(format!("{} is not a file", binary.display()));
    }
    // Absolute, because every subprocess runs with `current_dir` set to a scratch
    // project: a relative `--binary ./target/release/zuno` would resolve
    // against that scratch directory and report a bare "No such file".
    let binary = binary
        .canonicalize()
        .map_err(|source| format!("cannot resolve {}: {source}", binary.display()))?;
    Ok(Options {
        binary,
        cassette_root: cassette_root.unwrap_or_else(default_cassette_root),
        models: models.unwrap_or_else(default_models),
    })
}

/// The workspace root, derived from this crate's compile-time manifest directory.
///
/// Correct for the way this binary is used: it is built from this checkout and
/// run against an artifact, so the fixtures it needs are the ones next to the
/// source it was built from. Both defaults are overridable for the case where
/// they are not.
fn workspace_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .unwrap_or(manifest)
        .to_path_buf()
}

fn default_cassette_root() -> PathBuf {
    workspace_root().join("packaging/smoke/cassettes")
}

fn default_models() -> PathBuf {
    workspace_root().join("crates/zuno-llm/tests/fixtures/models-dev-pinned.json")
}

async fn run(options: &Options) -> Result<(), String> {
    check_version(options).await?;
    check_help(options).await?;
    check_tool_turn(options).await
}

/// Run the subject and wait for it, bounded.
///
/// `tokio::process` and not `std::process`: the mock provider's server runs on
/// this runtime, and a synchronous wait would stop driving it — the response
/// would never be written and the run would hang instead of failing. Costly
/// lesson from todo 104, kept here deliberately.
async fn invoke(
    options: &Options,
    arguments: &[&str],
    working_dir: &Path,
    variables: BTreeMap<String, String>,
) -> Result<Output, String> {
    let mut command = tokio::process::Command::new(&options.binary);
    command
        .args(arguments)
        .current_dir(working_dir)
        .env_clear()
        .envs(variables);
    match tokio::time::timeout(STEP_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(source)) => Err(format!(
            "could not launch `{} {}`: {source}",
            options.binary.display(),
            arguments.join(" ")
        )),
        Err(_) => Err(format!(
            "`{} {}` did not finish within {}s",
            options.binary.display(),
            arguments.join(" "),
            STEP_TIMEOUT.as_secs()
        )),
    }
}

fn describe(output: &Output) -> String {
    format!(
        "  exit: {:?}\n  stdout:\n{}\n  stderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).trim_end(),
        String::from_utf8_lossy(&output.stderr).trim_end()
    )
}

async fn check_version(options: &Options) -> Result<(), String> {
    let env = scripted_env()?;
    let output = invoke(options, &["--version"], env.working_dir(), env.env_vars()).await?;
    if !output.status.success() {
        return Err(format!(
            "`--version` exited non-zero\n{}",
            describe(&output)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // A binary that fails to start on the target platform commonly still exits 0
    // from a shell wrapper, so the *content* is asserted, not just the status.
    // `x.y.z` is the weakest shape that cannot be produced by an empty stdout or
    // by a loader error message.
    if !stdout.split_whitespace().any(is_version_triple) {
        return Err(format!(
            "`--version` printed no `x.y.z` version\n{}",
            describe(&output)
        ));
    }
    println!("zuno-smoke: ok    --version -> {}", stdout.trim());
    Ok(())
}

fn is_version_triple(token: &str) -> bool {
    let mut parts = token.trim_start_matches('v').split('.');
    let triple = [parts.next(), parts.next(), parts.next()];
    parts.next().is_none()
        && triple.iter().all(|part| {
            part.is_some_and(|part| {
                !part.is_empty() && part.chars().take_while(char::is_ascii_digit).count() > 0
            })
        })
}

async fn check_help(options: &Options) -> Result<(), String> {
    let env = scripted_env()?;
    let output = invoke(options, &["--help"], env.working_dir(), env.env_vars()).await?;
    if !output.status.success() {
        return Err(format!("`--help` exited non-zero\n{}", describe(&output)));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // `run` is the headless entry point the third check drives, so its absence
    // from the command tree would make that check's failure confusing.
    for expected in ["run", "serve"] {
        if !stdout.contains(expected) {
            return Err(format!(
                "`--help` did not list the `{expected}` command\n{}",
                describe(&output)
            ));
        }
    }
    println!("zuno-smoke: ok    --help lists run + serve");
    Ok(())
}

async fn check_tool_turn(options: &Options) -> Result<(), String> {
    let env = scripted_env()?;
    let cassette = load_cassette(&options.cassette_root)?;
    let title_response = cassette
        .http_interactions()
        .nth(1)
        .ok_or_else(|| {
            format!("{CASSETTE} has no recorded text continuation for the title prelude")
        })
        .and_then(|interaction| {
            MockResponse::from_recorded(CASSETTE, 2, interaction)
                .map_err(|source| format!("cannot replay {CASSETTE} interaction 2: {source}"))
        })?;
    let scenario = Scenario::new("artifact-smoke")
        .respond(title_response)
        .from_cassette(CASSETTE, &cassette)
        .map_err(|source| format!("cannot build a scenario from {CASSETTE}: {source}"))?;
    let provider = MockProvider::start(vec![scenario])
        .await
        .map_err(|source| format!("mock provider could not bind loopback: {source}"))?;
    if !provider.authored_scenarios().is_empty() {
        return Err(format!(
            "the smoke scenario serves authored bytes, so it proves nothing about the \
             wire format: {:?}",
            provider.authored_scenarios()
        ));
    }

    let variables = turn_variables(&env, provider.base_url(), &options.models);
    let outcome = invoke(
        options,
        &[
            "run",
            "--model",
            "test/test-model",
            "What is the weather in Paris?",
        ],
        env.working_dir(),
        variables,
    )
    .await;
    let captured = provider.captured().await;
    provider.shutdown().await;
    let output = outcome?;

    if !output.status.success() {
        return Err(format!(
            "the headless run exited non-zero\n{}",
            describe(&output)
        ));
    }
    let requests = captured
        .iter()
        .enumerate()
        .map(|(index, request)| {
            request.json().ok_or_else(|| {
                format!(
                    "provider request {} was not JSON: {}",
                    index + 1,
                    request.body
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let offered = validate_tool_turn_requests(&requests)
        .map_err(|failure| format!("{failure}\n{}", describe(&output)))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.contains(RECORDED_TOOL) {
        return Err(format!(
            "the unknown recorded tool `{RECORDED_TOOL}` was not reported\n{}",
            describe(&output)
        ));
    }
    println!(
        "zuno-smoke: ok    headless run replayed {CASSETTE}: {} requests, {} tools offered",
        captured.len(),
        offered.len()
    );
    Ok(())
}

fn validate_tool_turn_requests(requests: &[serde_json::Value]) -> Result<Vec<String>, String> {
    if requests.len() != 3 {
        return Err(format!(
            "the provider saw {} request(s), not 3; one tool turn must be one tool-free title \
             prelude plus two turn requests, and the tool result must return on the second turn \
             request",
            requests.len()
        ));
    }

    let prelude = &requests[0];
    let prelude_tools = advertised_tools(prelude);
    if !prelude_tools.is_empty() {
        return Err(format!(
            "the title prelude advertised {prelude_tools:?}, but the title agent denies every \
             tool and its request must stay tool-free:\n{prelude:#}"
        ));
    }
    if has_tool_result(prelude) {
        return Err(format!(
            "the title prelude carried a `role: tool` message even though it is the first \
             request on the wire:\n{prelude:#}"
        ));
    }

    let mut first_offered = Vec::new();
    for (turn_index, request) in requests[1..].iter().enumerate() {
        let offered = advertised_tools(request);
        for required in REQUIRED_TOOLS {
            if !offered.iter().any(|name| name == required) {
                let ordinal = if turn_index == 0 { "first" } else { "second" };
                return Err(format!(
                    "the {ordinal} turn request advertised {offered:?}, which does not include \
                     `{required}`; the assembled tool registry did not reach every real turn \
                     request:\n{request:#}"
                ));
            }
        }
        if turn_index == 0 {
            first_offered = offered;
        }
    }

    let second = &requests[2];
    if !has_tool_result(second) {
        return Err(format!(
            "the second turn request carries no `role: tool` message, so the tool loop did not \
             send its result back:\n{second:#}"
        ));
    }
    Ok(first_offered)
}

fn load_cassette(root: &Path) -> Result<zuno_testkit::Cassette, String> {
    let path = root.join(format!("{CASSETTE}.json"));
    let text = std::fs::read_to_string(&path)
        .map_err(|source| format!("cannot read {}: {source}", path.display()))?;
    zuno_testkit::Cassette::parse(&path, &text)
        .map_err(|source| format!("cannot parse {}: {source}", path.display()))
}

fn scripted_env() -> Result<ScriptedEnv, String> {
    ScriptedEnv::new().map_err(|source| format!("cannot build an isolated environment: {source}"))
}

/// A provider whose base URL is the loopback mock, declared inline so the subject
/// needs no config file on disk and no credentials.
fn provider_config(base_url: &str) -> String {
    serde_json::json!({
        "formatter": false,
        "lsp": false,
        "provider": {
            "test": {
                "name": "Test",
                "id": "test",
                "env": [],
                "transport": "openai-compatible",
                "api": format!("{base_url}/v1"),
                "models": {
                    "test-model": {
                        "id": "test-model",
                        "name": "Test Model",
                        "attachment": false,
                        "reasoning": false,
                        "temperature": false,
                        "tool_call": true,
                        "release_date": "2025-01-01",
                        "limit": { "context": 100_000, "output": 10_000 },
                        "cost": { "input": 0, "output": 0 },
                        "options": {}
                    }
                },
                "options": {
                    "apiKey": "test-key",
                    "baseURL": format!("{base_url}/v1")
                }
            }
        }
    })
    .to_string()
}

fn turn_variables(env: &ScriptedEnv, base_url: &str, models: &Path) -> BTreeMap<String, String> {
    let mut variables = env.env_vars();
    variables.extend([
        ("NO_COLOR".to_owned(), "1".to_owned()),
        ("TERM".to_owned(), "dumb".to_owned()),
        ("ZUNO_AUTH_CONTENT".to_owned(), "{}".to_owned()),
        ("ZUNO_DISABLE_MODELS_FETCH".to_owned(), "true".to_owned()),
        (
            "ZUNO_MODELS_PATH".to_owned(),
            models.to_string_lossy().into_owned(),
        ),
        ("ZUNO_CONFIG_CONTENT".to_owned(), provider_config(base_url)),
    ]);
    variables
}

fn advertised_tools(body: &serde_json::Value) -> Vec<String> {
    body.get("tools")
        .and_then(serde_json::Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    tool.pointer("/function/name")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn has_tool_result(body: &serde_json::Value) -> bool {
    body.get("messages")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|messages| {
            messages.iter().any(|message| {
                message.get("role").and_then(serde_json::Value::as_str) == Some("tool")
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn_request(messages: serde_json::Value) -> serde_json::Value {
        let tools = REQUIRED_TOOLS.map(|name| {
            serde_json::json!({
                "type": "function",
                "function": { "name": name }
            })
        });
        serde_json::json!({ "messages": messages, "tools": tools })
    }

    fn one_tool_turn() -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({ "messages": [] }),
            turn_request(serde_json::json!([{ "role": "user" }])),
            turn_request(serde_json::json!([{ "role": "tool" }])),
        ]
    }

    #[test]
    fn tool_turn_shape_includes_a_tool_free_title_prelude_and_two_registry_requests() {
        let requests = one_tool_turn();
        let offered = validate_tool_turn_requests(&requests)
            .expect("one tool turn is a tool-free title prelude plus two turn requests");
        assert_eq!(offered, REQUIRED_TOOLS);
    }

    #[test]
    fn title_prelude_must_not_advertise_the_tool_registry() {
        let mut requests = one_tool_turn();
        requests[0]["tools"] = requests[1]["tools"].clone();
        let failure = validate_tool_turn_requests(&requests)
            .expect_err("the title agent denies every tool, so its request must stay tool-free");
        assert!(failure.contains("title prelude"), "{failure}");
    }

    #[test]
    fn both_real_turn_requests_must_advertise_the_registry() {
        let mut requests = one_tool_turn();
        requests[2]["tools"] = serde_json::json!([]);
        let failure = validate_tool_turn_requests(&requests)
            .expect_err("the follow-up turn request must keep the assembled registry");
        assert!(failure.contains("second turn request"), "{failure}");
    }
}

//! Production-path proofs for the top-level `model` setting.
//!
//! A resolver-only test cannot prove which provider the process actually dials. Every
//! case here launches the real binary with two live loopback providers and asserts on
//! their independently captured requests. The configured cases swap the physical
//! ALPHA/BETA endpoints while keeping `model: "zzz/zzz-model"`; a catalog-first
//! implementation therefore fails both directions instead of passing by coincidence.
//! The unset case protects the deterministic catalog fallback this fix must preserve.

use std::collections::BTreeMap;
#[cfg(unix)]
use std::io::Read as _;
use std::path::PathBuf;
use std::process::Output;
#[cfg(unix)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::sync::mpsc;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

use zuno_testkit::{MockProvider, MockResponse, Scenario, ScriptedEnv, trusted_platform_config};

const RUN_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(unix)]
const TUI_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(unix)]
const VIEWPORT_ROWS: u16 = 30;
#[cfg(unix)]
const VIEWPORT_COLUMNS: u16 = 100;

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zuno"))
}

#[derive(Clone, Copy, Debug)]
enum Surface {
    Cli,
    #[cfg(unix)]
    Tui,
}

/// A minimal, deliberately authored completion used only as a physical route marker.
///
/// Provider wire compatibility is covered by cassette tests elsewhere. This response
/// exists so ALPHA and BETA are visibly distinguishable while both remain valid live
/// endpoints; the routing claim itself is proved by each server's captured requests.
fn route_response(label: &str, phase: &str) -> MockResponse {
    let marker = format!("ROUTE-{label}-{phase}");
    let chunk = serde_json::json!({
        "id": format!("chatcmpl-{label}-{phase}"),
        "object": "chat.completion.chunk",
        "created": 1,
        "model": format!("{label}-model"),
        "choices": [{
            "index": 0,
            "delta": {"role": "assistant", "content": marker},
            "finish_reason": null
        }]
    });
    let finish = serde_json::json!({
        "id": format!("chatcmpl-{label}-{phase}"),
        "object": "chat.completion.chunk",
        "created": 1,
        "model": format!("{label}-model"),
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    });
    MockResponse::authored(
        200,
        "text/event-stream",
        format!("data: {chunk}\n\ndata: {finish}\n\ndata: [DONE]\n\n"),
        "synthetic route marker; provider wire parity is not under test",
    )
}

async fn route_provider(label: &str) -> MockProvider {
    let scenario = Scenario::new(format!("route-{label}"))
        .respond(route_response(label, "TITLE"))
        .respond(route_response(label, "TURN"));
    MockProvider::start(vec![scenario])
        .await
        .expect("route provider binds loopback")
}

fn provider(label: &str, model: &str, base_url: &str) -> serde_json::Value {
    let mut models = serde_json::Map::new();
    models.insert(
        model.to_owned(),
        serde_json::json!({
            "id": model,
            "name": model,
            "attachment": false,
            "reasoning": false,
            "temperature": false,
            "tool_call": true,
            "release_date": "2025-01-01",
            "limit": {"context": 100_000, "output": 10_000},
            "cost": {"input": 0, "output": 0},
            "options": {}
        }),
    );
    serde_json::json!({
        "name": label,
        "id": label,
        "env": [],
        "transport": "openai-compatible",
        "models": models,
        "options": {"apiKey": "route-probe", "baseURL": format!("{base_url}/v1")}
    })
}

fn config(configured: Option<&str>, aaa_base_url: &str, zzz_base_url: &str) -> String {
    let mut value = serde_json::json!({
        "formatter": false,
        "lsp": false,
        "provider": {
            "aaa": provider("aaa", "aaa-model", aaa_base_url),
            "zzz": provider("zzz", "zzz-model", zzz_base_url)
        }
    });
    if let Some(configured) = configured {
        value["model"] = serde_json::json!(configured);
    }
    trusted_platform_config(value).to_string()
}

fn preset_config(aaa_base_url: &str, zzz_base_url: &str) -> String {
    let mut value: serde_json::Value =
        serde_json::from_str(&config(None, aaa_base_url, zzz_base_url))
            .expect("base route config is valid JSON");
    value["preset"] = serde_json::json!("team");
    value["presets"] = serde_json::json!({
        "team": {
            "agents": {
                "orchestrator": "zzz/zzz-model"
            }
        }
    });
    value.to_string()
}

fn variables(env: &ScriptedEnv, config: String) -> BTreeMap<String, String> {
    let mut variables = env.env_vars().into_iter().collect::<BTreeMap<_, _>>();
    variables.extend([
        ("NO_COLOR".to_owned(), "1".to_owned()),
        ("TERM".to_owned(), "xterm-256color".to_owned()),
        ("ZUNO_AUTH_CONTENT".to_owned(), "{}".to_owned()),
        ("ZUNO_DISABLE_MODELS_FETCH".to_owned(), "true".to_owned()),
        ("ZUNO_CONFIG_CONTENT".to_owned(), config),
    ]);
    variables
}

async fn run_cli(env: &ScriptedEnv, variables: BTreeMap<String, String>) -> Output {
    let mut command = tokio::process::Command::new(binary());
    command
        .args(["run", "What route is selected?"])
        .current_dir(env.working_dir())
        .env_clear()
        .envs(variables);
    tokio::time::timeout(RUN_TIMEOUT, command.output())
        .await
        .expect("the CLI route probe must finish inside its budget")
        .expect("launch the production CLI")
}

#[cfg(unix)]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(unix)]
fn run_tui(
    env: &ScriptedEnv,
    variables: BTreeMap<String, String>,
    wanted: &str,
) -> Result<String, std::io::Error> {
    let script = which::which("script").map_err(|_| {
        std::io::Error::other("`script` is required to give the TUI a real PTY; install util-linux")
    })?;
    let command = format!(
        "stty rows {VIEWPORT_ROWS} cols {VIEWPORT_COLUMNS}; {} --prompt 'What route is selected?' --auto",
        shell_quote(&binary().to_string_lossy())
    );
    let mut child = Command::new(script)
        .args(["-qefc", command.as_str(), "/dev/null"])
        .current_dir(env.working_dir())
        .env_clear()
        .envs(variables)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut output = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("the PTY launcher's stdout was not piped"))?;
    let (chunks, received) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        loop {
            match output.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(read) => {
                    if chunks.send(buffer[..read].to_vec()).is_err() {
                        return;
                    }
                }
            }
        }
    });

    let started = Instant::now();
    let mut transcript = String::new();
    while started.elapsed() < TUI_TIMEOUT {
        match received.recv_timeout(Duration::from_millis(250)) {
            Ok(chunk) => transcript.push_str(&String::from_utf8_lossy(&chunk)),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if transcript.contains(wanted) {
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    Ok(transcript)
}

struct RouteOutcome {
    aaa_requests: usize,
    zzz_requests: usize,
    output: String,
}

async fn route_once(
    surface: Surface,
    configured: Option<&str>,
    aaa: &MockProvider,
    zzz: &MockProvider,
    wanted: &str,
) -> RouteOutcome {
    route_with_config(
        surface,
        config(configured, aaa.base_url(), zzz.base_url()),
        aaa,
        zzz,
        wanted,
    )
    .await
}

async fn route_with_config(
    surface: Surface,
    config: String,
    aaa: &MockProvider,
    zzz: &MockProvider,
    wanted: &str,
) -> RouteOutcome {
    #[cfg(not(unix))]
    let _ = wanted;

    let env = ScriptedEnv::new().expect("isolated environment");
    let variables = variables(&env, config);
    let aaa_before = aaa.captured().await.len();
    let zzz_before = zzz.captured().await.len();
    let output = match surface {
        Surface::Cli => {
            let output = run_cli(&env, variables).await;
            assert!(
                output.status.success(),
                "production CLI failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).into_owned()
        }
        #[cfg(unix)]
        Surface::Tui => {
            let wanted = wanted.to_owned();
            tokio::task::spawn_blocking(move || run_tui(&env, variables, &wanted))
                .await
                .expect("join PTY route probe")
                .expect("launch production TUI")
        }
    };
    RouteOutcome {
        aaa_requests: aaa.captured().await.len() - aaa_before,
        zzz_requests: zzz.captured().await.len() - zzz_before,
        output,
    }
}

async fn assert_active_preset_routes_the_default_agent() {
    let aaa = route_provider("PRESET-FALLBACK").await;
    let zzz = route_provider("PRESET-SELECTED").await;
    let expected = "ROUTE-PRESET-SELECTED-TURN";
    let outcome = route_with_config(
        Surface::Cli,
        preset_config(aaa.base_url(), zzz.base_url()),
        &aaa,
        &zzz,
        expected,
    )
    .await;

    assert_eq!(
        outcome.aaa_requests, 0,
        "the active Preset was ignored and the catalog fallback provider was dialled"
    );
    assert_eq!(
        outcome.zzz_requests, 2,
        "the Preset-selected model must receive both title and foreground turn requests"
    );
    assert!(outcome.output.contains(expected));
    aaa.shutdown().await;
    zzz.shutdown().await;
}

async fn assert_configured_model(surface: Surface, aaa_label: &str, zzz_label: &str) {
    let aaa = route_provider(aaa_label).await;
    let zzz = route_provider(zzz_label).await;
    let expected = format!("ROUTE-{zzz_label}-TURN");
    let outcome = route_once(surface, Some("zzz/zzz-model"), &aaa, &zzz, &expected).await;

    assert_eq!(
        outcome.aaa_requests, 0,
        "{surface:?} ignored config.model and dialled catalog-first aaa ({aaa_label}); output:\n{}",
        outcome.output
    );
    assert_eq!(
        outcome.zzz_requests, 2,
        "{surface:?} did not send the title and turn to configured zzz ({zzz_label}); output:\n{}",
        outcome.output
    );
    assert!(
        outcome.output.contains(&expected),
        "{surface:?} did not render the configured provider's distinguishable reply {expected:?}"
    );
    aaa.shutdown().await;
    zzz.shutdown().await;
}

async fn assert_deterministic_fallback(surface: Surface) {
    let aaa = route_provider("FALLBACK").await;
    let zzz = route_provider("NONFALLBACK").await;
    let outcome = route_once(surface, None, &aaa, &zzz, "ROUTE-FALLBACK-TURN").await;
    assert_eq!(
        outcome.aaa_requests, 2,
        "{surface:?} changed the deterministic aaa fallback; output:\n{}",
        outcome.output
    );
    assert_eq!(
        outcome.zzz_requests, 0,
        "{surface:?} dialled zzz even though unset fallback must choose aaa"
    );
    assert!(outcome.output.contains("ROUTE-FALLBACK-TURN"));
    aaa.shutdown().await;
    zzz.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_honors_configured_model_when_zzz_is_beta() {
    assert_configured_model(Surface::Cli, "ALPHA", "BETA").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_honors_configured_model_when_zzz_is_alpha() {
    assert_configured_model(Surface::Cli, "BETA", "ALPHA").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[cfg(unix)]
async fn tui_honors_configured_model_when_zzz_is_beta() {
    assert_configured_model(Surface::Tui, "ALPHA", "BETA").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[cfg(unix)]
async fn tui_honors_configured_model_when_zzz_is_alpha() {
    assert_configured_model(Surface::Tui, "BETA", "ALPHA").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_keeps_deterministic_catalog_fallback_when_model_is_unset() {
    assert_deterministic_fallback(Surface::Cli).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_routes_the_default_orchestrator_through_the_active_preset() {
    assert_active_preset_routes_the_default_agent().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[cfg(unix)]
async fn tui_keeps_deterministic_catalog_fallback_when_model_is_unset() {
    assert_deterministic_fallback(Surface::Tui).await;
}

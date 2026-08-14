//! The proof a user can roll back to the released binary after a Rust turn.
//!
//! # What this exists to catch that nothing else could
//!
//! Success criterion 1 — "side-by-side use and rollback are real rather than
//! claimed" — is a claim about a *seam*: rows this port writes must stay decodable
//! by the release a user reverts to. Every test on either side of that seam passed
//! while the seam itself was broken. `crates/oc-db/tests/schema.rs` compared the
//! schema, `compat_suite.rs` round-tripped the migration journal, and this crate's
//! turn tests drove a real turn — none of them asked the released binary to *decode
//! a session this port wrote*.
//!
//! It could not: a session row spelled `{"providerID","modelID"}` writes and reads
//! back through this port perfectly. Only upstream's decoder, which reads
//! `row.model.id` (`packages/opencode/src/session/session.ts:88-93`), rejects it —
//! with `Expected string, got undefined` and exit 1, for the whole listing, not just
//! the one row.
//!
//! So this test does the one thing the others do not: it runs a **real turn through
//! the production binary**, then hands the resulting database to the **installed
//! pinned release** and requires it to list the session. Both halves are the real
//! programs; neither side is a fixture.
//!
//! # The skip contract
//!
//! The released binary is a machine-local install, so its absence is announced on
//! stderr rather than passing quietly. A silent skip reporting green is the failure
//! mode this file exists to prevent.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use oc_testkit::env::DbChoice;
use oc_testkit::{MockProvider, Scenario, ScriptedEnv, pinned_oracle_or_skip};

/// A recorded tool-free text completion. A new session titles itself before its
/// first turn, so one run replays this twice.
const CASSETTE: &str = "openai-chat/streams-text";

/// Wall-clock budget for one cassette-backed run: everything it talks to is
/// loopback, so exceeding this is a hang rather than slowness.
const RUN_TIMEOUT: Duration = Duration::from_secs(30);

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zuno"))
}

/// A config naming one OpenAI-compatible provider pointed at the mock.
///
/// The endpoint lives only in `options.baseURL`, the shape the upstream docs show —
/// see the note at `tests/tool_turn.rs:90-95` for why a top-level `api` here hid a
/// defect for several waves.
fn provider_config(base_url: &str) -> String {
    serde_json::json!({
        "formatter": false,
        "lsp": false,
        "provider": {
            "test": {
                "name": "Test",
                "id": "test",
                "env": [],
                "npm": "@ai-sdk/openai-compatible",
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

fn variables(env: &ScriptedEnv, base_url: &str) -> BTreeMap<String, String> {
    let mut variables = env
        .env_vars()
        .into_iter()
        .map(|(key, value)| (oc_paths::env::accepted_env_name(&key).to_owned(), value))
        .collect::<BTreeMap<_, _>>();
    variables.extend([
        ("NO_COLOR".to_owned(), "1".to_owned()),
        ("TERM".to_owned(), "dumb".to_owned()),
        ("ZUNO_PURE".to_owned(), "1".to_owned()),
        ("ZUNO_AUTH_CONTENT".to_owned(), "{}".to_owned()),
        ("ZUNO_DISABLE_MODELS_FETCH".to_owned(), "true".to_owned()),
        (
            "OPENCODE_CONFIG_CONTENT".to_owned(),
            provider_config(base_url),
        ),
    ]);
    variables
}

/// Launch the production binary and wait for it, bounded.
///
/// `tokio::process` rather than `std::process` because the mock provider runs on
/// this test's runtime: a synchronous wait would stop driving it and the run would
/// hang rather than fail.
async fn run_prompt(env: &ScriptedEnv, base_url: &str, prompt: &str) -> Output {
    let mut command = tokio::process::Command::new(binary());
    command
        .args(["run", "--model", "test/test-model", prompt])
        .current_dir(env.working_dir())
        .env_clear()
        .envs(variables(env, base_url));
    tokio::time::timeout(RUN_TIMEOUT, command.output())
        .await
        .expect("the run must finish inside its budget")
        .expect("launch zuno run")
}

/// Ask the released binary to list the sessions in `database`.
///
/// The environment is cleared apart from `PATH` — the release is a Bun program and
/// needs its own runtime — so the only thing steering it is `OPENCODE_DB`. Anything
/// inherited from the host could point it at a different database and turn a broken
/// seam into a pass.
fn oracle_session_list(oracle: &Path, database: &Path, home: &Path) -> Output {
    let mut command = std::process::Command::new(oracle);
    command
        .args(["session", "list", "--format", "json"])
        .current_dir(home)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("OPENCODE_DB", database)
        .env("OPENCODE_DISABLE_AUTOUPDATE", "1");
    command.output().expect("run the released opencode binary")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_released_binary_lists_a_session_this_port_wrote() {
    let Some(oracle) = pinned_oracle_or_skip(
        "the_released_binary_lists_a_session_this_port_wrote",
        "the rollback seam was NOT tested",
    ) else {
        return;
    };

    let env = ScriptedEnv::new().expect("isolated environment");
    let database = env.xdg_data().join("zuno").join("rollback.db");
    let env = env.with_db(DbChoice::Absolute(database.clone()));

    let scenario = Scenario::new("rollback-text-turn")
        .from_oracle_cassette(CASSETTE)
        .expect("the recorded title completion loads")
        .from_oracle_cassette(CASSETTE)
        .expect("the recorded turn completion loads");
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");
    assert!(
        provider.authored_scenarios().is_empty(),
        "this test must replay recorded provider bytes only"
    );

    let output = run_prompt(&env, provider.base_url(), "say hello").await;
    provider.shutdown().await;
    assert!(
        output.status.success(),
        "the Rust turn itself failed, so there is no written session to roll back \
         to\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        database.is_file(),
        "the turn wrote no database at {}",
        database.display()
    );

    let written = {
        let connection = oc_db::Connection::open(&database).expect("open the written database");
        let mut statement = connection
            .prepare("SELECT id, model FROM session ORDER BY rowid")
            .expect("prepare the session query");
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .expect("query the sessions")
            .collect::<Result<Vec<_>, _>>()
            .expect("read the sessions")
    };
    assert_eq!(
        written.len(),
        1,
        "one turn must leave exactly one session: {written:?}"
    );
    let (session_id, model) = &written[0];
    let model = model
        .as_ref()
        .expect("a turn records the model it ran under");

    let list = oracle_session_list(oracle, &database, env.home());
    let stdout = String::from_utf8_lossy(&list.stdout);
    let stderr = String::from_utf8_lossy(&list.stderr);
    eprintln!(
        "rollback: released opencode exited {}\n  session={session_id}\n  \
         session.model={model}\n  stdout={}\n  stderr={}",
        list.status,
        stdout.trim(),
        stderr.trim()
    );

    assert!(
        list.status.success(),
        "the released binary could not read a session this port wrote. \
         `session.model` was {model}; upstream decodes it as `row.model.id` \
         (session.ts:88-93), so a row spelled `modelID` produces `Expected string, \
         got undefined` and takes the whole listing down with it.\nexit: {}\n\
         stdout:\n{stdout}\nstderr:\n{stderr}",
        list.status
    );
    assert!(
        stdout.contains(session_id.as_str()),
        "the released binary exited 0 but did not list the session this port \
         wrote:\nstdout:\n{stdout}"
    );
}

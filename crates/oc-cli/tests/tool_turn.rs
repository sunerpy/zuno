//! The proof the binary can execute a turn in which the model calls a tool.
//!
//! Todo 44 tested that the registry assembles the right tool set and wave 6 tested
//! that `run_turn` drives a tool loop, and both stayed green while `run` passed the
//! dispatcher an empty tool vector. Neither test could see that, because neither
//! went through a production entry point. These do: they launch the real
//! `opencode-rust run` binary against a cassette-backed provider and assert on what
//! the binary put on the wire and what it did to the filesystem.
//!
//! # Why two tests and not one
//!
//! [`tool_turn_offers_the_assembled_registry_and_continues_after_a_tool_result`]
//! replays `openai-chat/drives-a-tool-loop-end-to-end` byte for byte. Its recorded
//! call names `get_weather`, a tool this runtime does not have, so it proves the
//! two properties that do not need a matching implementation: the request carries
//! the assembled registry, and an unknown call still produces a tool result that
//! the loop sends back. Those are recorded provider bytes, so nothing about the
//! wire format is this repository's opinion.
//!
//! [`tool_turn_executes_a_real_tool_and_the_side_effect_lands_on_disk`] needs the
//! model to call a tool that exists, and no recording of this runtime's own tool
//! names can exist yet. It therefore rewrites the recorded stream's tool name and
//! arguments and declares the result authored, which is exactly the accounting
//! `oc-testkit` exists to force: the framing, chunk boundaries, finish reason and
//! usage frame are still the recorded ones, and only the two values that name a
//! tool are this repository's.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use oc_testkit::mock_provider::{MockResponse, ResponseOrigin};
use oc_testkit::{CassettePlayer, DbChoice, MockProvider, Scenario, ScriptedEnv};

/// The recorded conversation both tests build on.
const CASSETTE: &str = "openai-chat/drives-a-tool-loop-end-to-end";

/// The recorded tool-free text completion that answers the prelude's title request.
///
/// The same recording todo 88's frozen harness uses for the same purpose
/// (`crates/oc-testkit/src/perf/workload.rs:93`), for the same reason: every run now
/// opens with exactly one tool-free request, because a new session generates its title
/// before its first real turn. That is what makes the harness's
/// `completed_tool_turns(captured) = (captured - PRELUDE_REQUESTS) / RESPONSES_PER_TURN`
/// come out at 1 for one tool turn instead of 0.
const TITLE_CASSETTE: &str = "openai-chat/streams-text";

/// `PRELUDE_REQUESTS` and `RESPONSES_PER_TURN` from the frozen harness, in that order.
///
/// Duplicated here rather than imported because `oc-testkit`'s copies are
/// crate-private and deliberately frozen — importing them would let a future edit to
/// one silently retune this assertion, which is the massaged pass three earlier agents
/// declined to produce. If these ever disagree with `perf/workload.rs:18-19`, the port
/// has changed its request shape and the perf gate is no longer measuring what it
/// measured from real 1.18.12 traffic.
const FROZEN_PRELUDE_REQUESTS: usize = 1;
const FROZEN_RESPONSES_PER_TURN: usize = 2;

/// Total provider requests one tool turn must produce for the frozen gate to score it.
const REQUESTS_FOR_ONE_TOOL_TURN: usize = FROZEN_PRELUDE_REQUESTS + FROZEN_RESPONSES_PER_TURN;

/// The tool the recording calls, which this runtime deliberately does not have.
const RECORDED_TOOL: &str = "get_weather";

/// Tools the assembled registry must offer a non-GPT model.
///
/// Not the whole list on purpose: `edit` and `write` are model-conditional and
/// `websearch` is provider-conditional, so pinning every id here would make this
/// test fail for a reason that has nothing to do with the wiring it exists to
/// check. These four are unconditional for every model and provider.
const REQUIRED_TOOLS: [&str; 4] = ["bash", "read", "glob", "grep"];

/// Wall-clock budget for one cassette-backed run. Everything it talks to is
/// loopback or the local filesystem, so exceeding this is a hang, not slowness.
const RUN_TIMEOUT: Duration = Duration::from_secs(30);

/// What the executed `write` call must leave on disk.
const WRITTEN_CONTENT: &str = "the tool ran\n";
const ANTIGRAVITY_SPEC: &str = "opencode-antigravity-auth@1.6.0";
const ANTIGRAVITY_TOOL: &str = "google_search";

const FAILING_AUTH_LOADER_PLUGIN: &str = r#"
export default {
  id: "cli-failing-auth-loader",
  server: async () => ({
    auth: {
      provider: "test",
      loader: async (getAuth) => {
        await getAuth();
        throw new Error("task173 CLI auth loader failure");
      },
      methods: [],
    },
  }),
};
"#;

const AUTO_DISCOVERY_PLUGIN: &str = r#"
import { appendFileSync } from "node:fs";

export default {
  id: "production-auto-discovery-fixture",
  server: async () => {
    appendFileSync(process.env.AUTO_DISCOVERY_LOG, `${import.meta.url.split("/").pop()}\n`);
    return {};
  },
};
"#;

/// A provider hook written against `@opencode-ai/plugin` 1.18.15's
/// `Record<string, ModelV2>` contract. It constructs the SDK value from scratch:
/// in particular, it emits `providerID` and omits the SDK-optional `family` and
/// `variants` fields rather than spreading the Rust host's provider input back.
const SDK_MODEL_PROVIDER_PLUGIN: &str = r#"
export default {
  id: "production-sdk-model-provider",
  server: async (_input, options) => ({
    provider: {
      id: "github-copilot",
      models: async () => ({
        [options.catalogID]: {
          id: options.catalogID,
          providerID: "github-copilot",
          api: {
            id: options.apiID,
            url: `${options.baseURL}/v1`,
            npm: "@ai-sdk/github-copilot",
            endpoint: options.endpoint,
          },
          name: "SDK model provider fixture",
          capabilities: {
            temperature: true,
            reasoning: false,
            attachment: false,
            toolcall: true,
            input: { text: true, audio: false, image: false, video: false, pdf: false },
            output: { text: true, audio: false, image: false, video: false, pdf: false },
            interleaved: false,
          },
          cost: { input: 0, output: 0, cache: { read: 0, write: 0 } },
          limit: { context: 100000, output: 8192 },
          status: "active",
          options: {},
          headers: {},
          release_date: "2026-08-12",
        },
        malformed: { providerID: "github-copilot" },
      }),
    },
  }),
};
"#;

const LIFECYCLE_PLUGIN: &str = r#"
import { appendFileSync } from "node:fs";

export default {
  id: "production-lifecycle-fixture",
  server: async (_input, options) => ({
    config: async (config) => {
      config.command.lifecycle.template = "config:$ARGUMENTS";
    },
    tool: {
      lifecycle_tool: {
        description: "resource-hook-description",
        args: { value: {} },
        execute: async (args) => args.value,
      },
    },
    auth: {
      provider: "test",
      loader: async () => ({
        extraBody: { auth_hook_sentinel: "auth-hook" },
      }),
      methods: [],
    },
    provider: {
      id: "test",
      models: async (provider) => {
        provider.models["test-model"].api.id = "provider-hook-model";
        return provider.models;
      },
    },
    event: async (input) => {
      appendFileSync(options.eventFile, `${input.event.type}\n`);
    },
    "command.execute.before": async (input, output) => {
      output.parts[0].text += ":command";
    },
    "chat.message": async (input, output) => {
      appendFileSync(options.eventFile, `chat.message=${JSON.stringify({ input, message: output.message })}\n`);
      const expected = {
        id: input.messageID,
        sessionID: input.sessionID,
        agent: input.agent,
        model: input.model,
      };
      const missing = Object.keys(expected).filter((field) => output.message[field] === undefined);
      if (missing.length > 0) {
        output.parts[0].text += `:missing-chat-message-${missing.join("-")}`;
        return;
      }
      for (const [field, value] of Object.entries(expected)) {
        if (JSON.stringify(output.message[field]) !== JSON.stringify(value)) {
          output.parts[0].text += `:wrong-chat-message-${field}`;
          return;
        }
      }
      output.parts[0].text += ":chat";
    },
    "chat.params": async (_input, output) => {
      output.options.params_hook_sentinel = "chat-params-hook";
    },
    "chat.headers": async (_input, output) => {
      output.headers["x-chat-headers-hook"] = "chat-headers-hook";
    },
    "permission.ask": async (_input, output) => {
      output.status = "allow";
    },
    "tool.execute.before": async (input, output) => {
      if (input.tool === "bash") {
        output.args.command = "printf '%s' \"$PLUGIN_SHELL_ENV\"";
      }
      if (input.tool === "lifecycle_tool") {
        output.args.value = "before-hook";
      }
    },
    "shell.env": async (_input, output) => {
      output.env.PLUGIN_SHELL_ENV = "shell-env-hook";
    },
    "tool.execute.after": async (input, output) => {
      if (input.tool === "bash" || input.tool === "lifecycle_tool") {
        output.title = "after-hook-title";
        output.output += ":after-hook";
      }
    },
    "experimental.chat.messages.transform": async (_input, output) => {
      const user = output.messages.find((message) => message.info.role === "user");
      user.parts[0].text += ":messages";
    },
    "experimental.chat.system.transform": async (_input, output) => {
      output.system.push("system-hook-sentinel");
    },
    "experimental.provider.small_model": async (input, output) => {
      output.model = input.provider.models["small-model"];
    },
    "experimental.text.complete": async (_input, output) => {
      output.text += ":text-complete-hook";
    },
    "tool.definition": async (input, output) => {
      output.parameters = { type: "object", properties: {} };
      if (input.toolID === "lifecycle_tool") {
        output.description = "definition-hook-description";
      }
    },
    dispose: async () => appendFileSync(options.disposeFile, "disposed\n"),
  }),
};
"#;

const NOOP_TOOL_DEFINITION_PLUGIN: &str = r#"
import { appendFileSync } from "node:fs";

export default {
  id: "production-noop-tool-definition",
  server: async (_input, options) => ({
    "tool.definition": async (input, _output) => {
      appendFileSync(options.eventFile, `${input.toolID}\n`);
    },
  }),
};
"#;

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_opencode-rust"))
}

/// A config naming one OpenAI-compatible provider pointed at the mock.
///
/// It deliberately does **not** set `permission`. The turn must be governed by the
/// ruleset `agent list` prints, so an `"*": "allow"` override here would hide a
/// regression in which the real rules never reach the dispatcher.
///
/// It also deliberately does **not** set a top-level `api`. The endpoint lives only in
/// `options.baseURL`, which is the shape the upstream docs show and the shape todo 88's
/// frozen workload emits. The top-level key that used to be here was the same URL by
/// another name, and it is what hid todo 109 — the binary could not dial a provider
/// configured the documented way — through every wave that ran this test green.
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

fn plugin_provider_config(base_url: &str, deny_plugin_tool: bool) -> String {
    let mut config: serde_json::Value =
        serde_json::from_str(&provider_config(base_url)).expect("provider config is JSON");
    config["plugin"] = serde_json::json!([ANTIGRAVITY_SPEC]);
    if deny_plugin_tool {
        config["permission"] = serde_json::json!({ ANTIGRAVITY_TOOL: "deny" });
    }
    config.to_string()
}

fn failing_auth_loader_provider_config(base_url: &str, plugin: &Path) -> String {
    let mut config: serde_json::Value =
        serde_json::from_str(&provider_config(base_url)).expect("provider config is JSON");
    config["plugin"] = serde_json::json!([[format!("file:{}", plugin.display()), {}]]);
    config.to_string()
}

fn lifecycle_provider_config(
    base_url: &str,
    plugin: &Path,
    event_file: &Path,
    dispose_file: &Path,
) -> String {
    let mut config: serde_json::Value =
        serde_json::from_str(&provider_config(base_url)).expect("provider config is JSON");
    let provider = config["provider"]["test"]
        .as_object_mut()
        .expect("test provider is an object");
    provider
        .get_mut("models")
        .and_then(serde_json::Value::as_object_mut)
        .expect("test models are an object")
        .insert(
            "small-model".to_owned(),
            serde_json::json!({
                "id": "small-model",
                "name": "Small Model",
                "attachment": false,
                "reasoning": false,
                "temperature": false,
                "tool_call": false,
                "release_date": "2025-01-01",
                "limit": { "context": 100_000, "output": 10_000 },
                "cost": { "input": 0, "output": 0 },
                "options": {}
            }),
        );
    config["command"] = serde_json::json!({
        "lifecycle": { "template": "template:$ARGUMENTS" }
    });
    config["plugin"] = serde_json::json!([[
        format!("file:{}", plugin.display()),
        { "eventFile": event_file, "disposeFile": dispose_file }
    ]]);
    config.to_string()
}

fn noop_tool_definition_provider_config(
    base_url: &str,
    plugin: &Path,
    event_file: &Path,
) -> String {
    let mut config: serde_json::Value =
        serde_json::from_str(&provider_config(base_url)).expect("provider config is JSON");
    config["plugin"] = serde_json::json!([[
        format!("file:{}", plugin.display()),
        { "eventFile": event_file }
    ]]);
    config.to_string()
}

fn sdk_model_provider_config(
    base_url: &str,
    plugin: &Path,
    catalog_id: &str,
    api_id: &str,
    endpoint: &str,
) -> String {
    serde_json::json!({
        "formatter": false,
        "lsp": false,
        "provider": {
            "github-copilot": {
                "name": "GitHub Copilot",
                "id": "github-copilot",
                "env": [],
                "npm": "@ai-sdk/github-copilot",
                "models": {
                    catalog_id: {
                        "id": "base-model-before-plugin-replacement",
                        "name": "Base model before plugin replacement",
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
        },
        "plugin": [[
            format!("file:{}", plugin.display()),
            {
                "baseURL": base_url,
                "catalogID": catalog_id,
                "apiID": api_id,
                "endpoint": endpoint,
            }
        ]]
    })
    .to_string()
}

fn variables(env: &ScriptedEnv, base_url: &str) -> BTreeMap<String, String> {
    let mut variables = env.env_vars();
    variables.extend([
        ("NO_COLOR".to_owned(), "1".to_owned()),
        ("TERM".to_owned(), "dumb".to_owned()),
        ("OPENCODE_PURE".to_owned(), "1".to_owned()),
        ("OPENCODE_AUTH_CONTENT".to_owned(), "{}".to_owned()),
        // No `OPENCODE_MODELS_PATH`: the config below fully specifies `test/test-model`,
        // so a catalog is not needed to resolve it. Injecting a fixture here is what hid
        // todo 108 — the binary could not start without one — through five waves.
        (
            "OPENCODE_DISABLE_MODELS_FETCH".to_owned(),
            "true".to_owned(),
        ),
        (
            "OPENCODE_CONFIG_CONTENT".to_owned(),
            provider_config(base_url),
        ),
    ]);
    variables
}

/// Launch the real binary and wait for it, bounded.
///
/// `tokio::process` rather than `std::process` because the mock provider's server
/// runs on this test's runtime: a synchronous wait would stop driving it, the
/// response would never be written, and the run would hang rather than fail.
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
        .expect("launch opencode-rust run")
}

async fn run_plugin_prompt(
    env: &ScriptedEnv,
    base_url: &str,
    prompt: &str,
    deny_plugin_tool: bool,
) -> Output {
    let mut plugin_variables = variables(env, base_url);
    plugin_variables.remove("OPENCODE_PURE");
    plugin_variables.insert("XDG_CACHE_HOME".to_owned(), "/config/.cache".to_owned());
    plugin_variables.insert(
        "MISE_DATA_DIR".to_owned(),
        "/config/.local/share/mise".to_owned(),
    );
    plugin_variables.insert("PATH".to_owned(), "/usr/bin:/bin".to_owned());
    plugin_variables.insert(
        "OPENCODE_CONFIG_CONTENT".to_owned(),
        plugin_provider_config(base_url, deny_plugin_tool),
    );

    let mut command = tokio::process::Command::new(binary());
    command
        .args(["run", "--model", "test/test-model", prompt])
        .current_dir(env.working_dir())
        .env_clear()
        .envs(plugin_variables);
    tokio::time::timeout(Duration::from_secs(90), command.output())
        .await
        .expect("the plugin-backed run must finish inside its budget")
        .expect("launch plugin-backed opencode-rust run")
}

async fn run_failing_auth_loader_prompt(
    env: &ScriptedEnv,
    base_url: &str,
    plugin: &Path,
) -> Output {
    let mut plugin_variables = variables(env, base_url);
    plugin_variables.remove("OPENCODE_PURE");
    plugin_variables.insert("XDG_CACHE_HOME".to_owned(), "/config/.cache".to_owned());
    plugin_variables.insert(
        "MISE_DATA_DIR".to_owned(),
        "/config/.local/share/mise".to_owned(),
    );
    plugin_variables.insert("PATH".to_owned(), "/usr/bin:/bin".to_owned());
    plugin_variables.insert(
        "OPENCODE_CONFIG_CONTENT".to_owned(),
        failing_auth_loader_provider_config(base_url, plugin),
    );
    plugin_variables.insert(
        "OPENCODE_AUTH_CONTENT".to_owned(),
        r#"{"test":{"type":"api","key":"fixture-key"}}"#.to_owned(),
    );

    let mut command = tokio::process::Command::new(binary());
    command
        .args([
            "run",
            "--model",
            "test/test-model",
            "Continue after the auth loader fails.",
        ])
        .current_dir(env.working_dir())
        .env_clear()
        .envs(plugin_variables);
    tokio::time::timeout(Duration::from_secs(90), command.output())
        .await
        .expect("the failing-auth-loader run must finish inside its budget")
        .expect("launch failing-auth-loader opencode-rust run")
}

async fn run_sdk_model_provider_prompt(
    env: &ScriptedEnv,
    base_url: &str,
    plugin: &Path,
    catalog_id: &str,
    api_id: &str,
    endpoint: &str,
    print_debug_logs: bool,
) -> Output {
    let mut plugin_variables = variables(env, base_url);
    plugin_variables.remove("OPENCODE_PURE");
    plugin_variables.insert("XDG_CACHE_HOME".to_owned(), "/config/.cache".to_owned());
    plugin_variables.insert(
        "MISE_DATA_DIR".to_owned(),
        "/config/.local/share/mise".to_owned(),
    );
    plugin_variables.insert("PATH".to_owned(), "/usr/bin:/bin".to_owned());
    plugin_variables.insert(
        "OPENCODE_CONFIG_CONTENT".to_owned(),
        sdk_model_provider_config(base_url, plugin, catalog_id, api_id, endpoint),
    );

    let model = format!("github-copilot/{catalog_id}");
    let mut arguments = Vec::new();
    if print_debug_logs {
        arguments.extend(["--print-logs", "--log-level", "DEBUG"]);
    }
    arguments.extend([
        "run",
        "--model",
        &model,
        "Exercise the SDK model provider hook.",
    ]);

    let mut command = tokio::process::Command::new(binary());
    command
        .args(&arguments)
        .current_dir(env.working_dir())
        .env_clear()
        .envs(plugin_variables);
    tokio::time::timeout(Duration::from_secs(90), command.output())
        .await
        .expect("the SDK-model plugin run must finish inside its budget")
        .expect("launch SDK-model plugin run")
}

async fn run_auto_discovery_prompt(env: &ScriptedEnv, base_url: &str, load_log: &Path) -> Output {
    let mut plugin_variables = variables(env, base_url);
    plugin_variables.remove("OPENCODE_PURE");
    plugin_variables.insert("XDG_CACHE_HOME".to_owned(), "/config/.cache".to_owned());
    plugin_variables.insert(
        "MISE_DATA_DIR".to_owned(),
        "/config/.local/share/mise".to_owned(),
    );
    plugin_variables.insert("PATH".to_owned(), "/usr/bin:/bin".to_owned());
    plugin_variables.insert(
        "AUTO_DISCOVERY_LOG".to_owned(),
        load_log.display().to_string(),
    );
    plugin_variables.insert(
        "OPENCODE_CONFIG_DIR".to_owned(),
        env.project().join("broken-config").display().to_string(),
    );

    let mut command = tokio::process::Command::new(binary());
    command
        .args([
            "--print-logs",
            "--log-level",
            "DEBUG",
            "run",
            "--model",
            "test/test-model",
            "Load auto-discovered plugins.",
        ])
        .current_dir(env.working_dir())
        .env_clear()
        .envs(plugin_variables);
    tokio::time::timeout(Duration::from_secs(90), command.output())
        .await
        .expect("the auto-discovery run must finish inside its budget")
        .expect("launch auto-discovery run")
}

async fn run_lifecycle_command(
    env: &ScriptedEnv,
    base_url: &str,
    plugin: &Path,
    event_file: &Path,
    dispose_file: &Path,
) -> Output {
    run_lifecycle_command_with_args(
        env,
        base_url,
        plugin,
        event_file,
        dispose_file,
        &[
            "run",
            "--model",
            "test/test-model",
            "--command",
            "lifecycle",
            "raw arguments",
        ],
    )
    .await
}

async fn run_lifecycle_command_with_args(
    env: &ScriptedEnv,
    base_url: &str,
    plugin: &Path,
    event_file: &Path,
    dispose_file: &Path,
    args: &[&str],
) -> Output {
    let mut plugin_variables = variables(env, base_url);
    plugin_variables.remove("OPENCODE_PURE");
    plugin_variables.insert("XDG_CACHE_HOME".to_owned(), "/config/.cache".to_owned());
    plugin_variables.insert(
        "MISE_DATA_DIR".to_owned(),
        "/config/.local/share/mise".to_owned(),
    );
    plugin_variables.insert("PATH".to_owned(), "/usr/bin:/bin".to_owned());
    plugin_variables.insert(
        "OPENCODE_AUTH_CONTENT".to_owned(),
        r#"{"test":{"type":"api","key":"fixture-key"}}"#.to_owned(),
    );
    plugin_variables.insert(
        "OPENCODE_CONFIG_CONTENT".to_owned(),
        lifecycle_provider_config(base_url, plugin, event_file, dispose_file),
    );

    let mut command = tokio::process::Command::new(binary());
    command
        .args(args)
        .current_dir(env.working_dir())
        .env_clear()
        .envs(plugin_variables);
    tokio::time::timeout(Duration::from_secs(90), command.output())
        .await
        .expect("the lifecycle-plugin run must finish inside its budget")
        .expect("launch lifecycle-plugin run")
}

async fn run_noop_tool_definition_prompt(
    env: &ScriptedEnv,
    base_url: &str,
    plugin: &Path,
    event_file: &Path,
) -> Output {
    let mut plugin_variables = variables(env, base_url);
    plugin_variables.remove("OPENCODE_PURE");
    plugin_variables.insert("XDG_CACHE_HOME".to_owned(), "/config/.cache".to_owned());
    plugin_variables.insert(
        "MISE_DATA_DIR".to_owned(),
        "/config/.local/share/mise".to_owned(),
    );
    plugin_variables.insert("PATH".to_owned(), "/usr/bin:/bin".to_owned());
    plugin_variables.insert(
        "OPENCODE_CONFIG_CONTENT".to_owned(),
        noop_tool_definition_provider_config(base_url, plugin, event_file),
    );

    let mut command = tokio::process::Command::new(binary());
    command
        .args(["run", "--model", "test/test-model", "hello"])
        .current_dir(env.working_dir())
        .env_clear()
        .envs(plugin_variables);
    tokio::time::timeout(Duration::from_secs(90), command.output())
        .await
        .expect("the no-op tool.definition run must finish inside its budget")
        .expect("launch no-op tool.definition run")
}

async fn run_lifecycle_tool_prompt(
    env: &ScriptedEnv,
    base_url: &str,
    plugin: &Path,
    event_file: &Path,
    dispose_file: &Path,
) -> Output {
    let mut plugin_variables = variables(env, base_url);
    plugin_variables.remove("OPENCODE_PURE");
    plugin_variables.insert("XDG_CACHE_HOME".to_owned(), "/config/.cache".to_owned());
    plugin_variables.insert(
        "MISE_DATA_DIR".to_owned(),
        "/config/.local/share/mise".to_owned(),
    );
    plugin_variables.insert("PATH".to_owned(), "/usr/bin:/bin".to_owned());
    let mut config: serde_json::Value = serde_json::from_str(&lifecycle_provider_config(
        base_url,
        plugin,
        event_file,
        dispose_file,
    ))
    .expect("lifecycle provider config is JSON");
    config["permission"] = serde_json::json!({ "lifecycle_tool": "ask" });
    plugin_variables.insert("OPENCODE_CONFIG_CONTENT".to_owned(), config.to_string());

    let mut command = tokio::process::Command::new(binary());
    command
        .args(["run", "--model", "test/test-model", "Use the shell once."])
        .current_dir(env.working_dir())
        .env_clear()
        .envs(plugin_variables);
    tokio::time::timeout(Duration::from_secs(90), command.output())
        .await
        .expect("the lifecycle tool run must finish inside its budget")
        .expect("launch lifecycle tool run")
}

fn request_model(body: &serde_json::Value) -> Option<&str> {
    body.get("model").and_then(serde_json::Value::as_str)
}

fn request_contains_text(body: &serde_json::Value, expected: &str) -> bool {
    body.get("messages")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|messages| {
            messages.iter().any(|message| {
                message.get("content").is_some_and(|content| match content {
                    serde_json::Value::String(text) => text.contains(expected),
                    serde_json::Value::Array(parts) => parts.iter().any(|part| {
                        part.get("text")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|text| text.contains(expected))
                    }),
                    _ => false,
                })
            })
        })
}

/// Every `tools[].function.name` the binary advertised in a captured request.
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

fn advertised_tool_description<'a>(body: &'a serde_json::Value, id: &str) -> Option<&'a str> {
    body.get("tools")
        .and_then(serde_json::Value::as_array)?
        .iter()
        .find(|tool| {
            tool.pointer("/function/name")
                .and_then(serde_json::Value::as_str)
                == Some(id)
        })?
        .pointer("/function/description")
        .and_then(serde_json::Value::as_str)
}

fn bridge_truncation_paths(value: &serde_json::Value) -> Vec<String> {
    fn visit(value: &serde_json::Value, paths: &mut Vec<String>) {
        match value {
            serde_json::Value::Array(values) => {
                for value in values {
                    visit(value, paths);
                }
            }
            serde_json::Value::Object(map) => {
                if map.get("$truncated").and_then(serde_json::Value::as_bool) == Some(true) {
                    paths.push(
                        map.get("$path")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("/")
                            .to_owned(),
                    );
                }
                for value in map.values() {
                    visit(value, paths);
                }
            }
            serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::String(_) => {}
        }
    }

    let mut paths = Vec::new();
    visit(value, &mut paths);
    paths
}

/// The frozen harness's own arithmetic, reproduced so a failure names the number.
const fn completed_tool_turns(captured_requests: usize) -> usize {
    captured_requests.saturating_sub(FROZEN_PRELUDE_REQUESTS) / FROZEN_RESPONSES_PER_TURN
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

async fn assert_sdk_model_provider_endpoint(
    catalog_id: &str,
    api_id: &str,
    advertised_endpoint: &str,
    expected_path: &str,
    expected_body_key: &str,
    cassette: &str,
) {
    let env = ScriptedEnv::new().expect("isolated environment");
    let plugin = env.project().join("sdk-model-provider.mjs");
    std::fs::write(&plugin, SDK_MODEL_PROVIDER_PLUGIN).expect("write SDK model provider plugin");
    let scenario = Scenario::new(format!("sdk-model-{advertised_endpoint}"))
        .on_path(expected_path)
        .from_oracle_cassette(cassette)
        .expect("the recorded title response loads")
        .from_oracle_cassette(cassette)
        .expect("the recorded turn response loads");
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");

    let output = run_sdk_model_provider_prompt(
        &env,
        provider.base_url(),
        &plugin,
        catalog_id,
        api_id,
        advertised_endpoint,
        false,
    )
    .await;
    let captured = provider.captured().await;
    provider.shutdown().await;

    assert!(
        output.status.success(),
        "SDK-model plugin run failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(captured.len(), 2, "title plus one ordinary turn");
    let request = captured.last().expect("ordinary turn request");
    assert_eq!(request.path, expected_path);
    let body = request.json().expect("provider request is JSON");
    assert_eq!(request_model(&body), Some(api_id));
    assert!(
        body.get(expected_body_key).is_some(),
        "the advertised `{advertised_endpoint}` surface did not shape the request: {body:#}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_js_sdk_model_advertised_responses_beats_a_heuristic_hostile_id() {
    assert_sdk_model_provider_endpoint(
        "mai-code-alias",
        "mai-code-1-flash-picker",
        "responses",
        "/v1/responses",
        "input",
        "openai-responses/gpt-5-5-streams-text",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_js_sdk_model_advertised_chat_beats_a_responses_heuristic_id() {
    assert_sdk_model_provider_endpoint(
        "gpt-5-alias",
        "gpt-5",
        "chat",
        "/v1/chat/completions",
        "messages",
        "openai-compatible-chat/deepseek-streams-text",
    )
    .await;
}

/// The two tests above prove a malformed sibling is isolated, and both stay green when
/// the diagnostic that reports it is deleted. Since the drop is silent by design, that
/// event is the only way a plugin author learns their model was rejected, so this pins
/// the three facts it promises — plugin, rejected model id, decode reason — on one line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_js_malformed_model_diagnostic_names_the_plugin_model_and_decode_reason() {
    const MESSAGE: &str = "skipped a plugin model this host could not decode";

    let env = ScriptedEnv::new().expect("isolated environment");
    let plugin = env.project().join("sdk-model-provider.mjs");
    std::fs::write(&plugin, SDK_MODEL_PROVIDER_PLUGIN).expect("write SDK model provider plugin");
    let scenario = Scenario::new("sdk-model-malformed-sibling")
        .on_path("/v1/chat/completions")
        .from_oracle_cassette("openai-compatible-chat/deepseek-streams-text")
        .expect("the recorded title response loads")
        .from_oracle_cassette("openai-compatible-chat/deepseek-streams-text")
        .expect("the recorded turn response loads");
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");

    // The fixture returns one decodable model beside `malformed`, whose only field is
    // `providerID`. `plugin_model` renames that to `provider_id`, so `ResolvedModel`'s
    // first required field is what the decode reports missing.
    let output = run_sdk_model_provider_prompt(
        &env,
        provider.base_url(),
        &plugin,
        "gpt-5-alias",
        "gpt-5",
        "chat",
        true,
    )
    .await;
    let captured = provider.captured().await;
    provider.shutdown().await;

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "a malformed sibling must not fail the run\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout)
    );

    // Isolation, restated here so a future change cannot satisfy the diagnostic by
    // discarding the provider: the valid sibling still resolved and still dispatched.
    assert_eq!(
        captured.len(),
        2,
        "title plus one ordinary turn; stderr:\n{stderr}"
    );
    let request = captured.last().expect("ordinary turn request");
    assert_eq!(request.path, "/v1/chat/completions");
    let body = request.json().expect("provider request is JSON");
    assert_eq!(request_model(&body), Some("gpt-5"));

    // One line must carry all three facts. Matching the line first, then its fields,
    // is what makes this a content assertion: a diagnostic that fires with an empty
    // plugin, model, or error fails below instead of passing on presence alone.
    let diagnostic = stderr
        .lines()
        .find(|line| line.contains(MESSAGE))
        .unwrap_or_else(|| {
            panic!("the skipped model must be reported at DEBUG; stderr:\n{stderr}")
        });
    for (fact, expected) in [
        (
            "the plugin it came from",
            format!("file:{}", plugin.display()),
        ),
        ("the rejected model's id", "malformed".to_owned()),
        ("the decode reason", "missing field `id`".to_owned()),
    ] {
        assert!(
            diagnostic.contains(&expected),
            "the diagnostic must name {fact} ({expected:?})\nline: {diagnostic}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_turn_offers_the_assembled_registry_and_continues_after_a_tool_result() {
    let env = ScriptedEnv::new().expect("isolated environment");
    let scenario = Scenario::new("recorded-tool-loop")
        .from_oracle_cassette(TITLE_CASSETTE)
        .expect("the recorded text completion loads")
        .from_oracle_cassette(CASSETTE)
        .expect("the recorded tool loop loads");
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");
    assert!(
        provider.authored_scenarios().is_empty(),
        "this test must replay recorded provider bytes only"
    );

    let output = run_prompt(&env, provider.base_url(), "What is the weather in Paris?").await;
    let captured = provider.captured().await;
    provider.shutdown().await;

    assert!(
        output.status.success(),
        "run failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        captured.len(),
        REQUESTS_FOR_ONE_TOOL_TURN,
        "one tool turn must be one tool-free prelude request plus two turn requests; \
         the frozen gate scores {} completed turn(s) from {} captured",
        completed_tool_turns(captured.len()),
        captured.len()
    );
    assert_eq!(
        completed_tool_turns(captured.len()),
        1,
        "the frozen perf harness would still score this turn as incomplete"
    );

    let prelude = captured[0].json().expect("the prelude request is JSON");
    assert!(
        advertised_tools(&prelude).is_empty(),
        "the prelude request offered tools, but the title agent denies every one of \
         them:\nbody:\n{prelude:#}"
    );
    assert!(
        !has_tool_result(&prelude),
        "the prelude request is the first thing on the wire and cannot carry a tool \
         result:\n{prelude:#}"
    );

    let first = captured[1].json().expect("the first turn request is JSON");
    let offered = advertised_tools(&first);
    for required in REQUIRED_TOOLS {
        assert!(
            offered.iter().any(|name| name == required),
            "the request advertised {offered:?}, which does not include `{required}`; \
             the assembled registry did not reach the dispatcher\nbody:\n{first:#}"
        );
    }

    let second = captured[2].json().expect("the second turn request is JSON");
    assert!(
        has_tool_result(&second),
        "the second request carries no tool result:\n{second:#}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(RECORDED_TOOL),
        "the unknown recorded tool was not reported on stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Rewrite the recorded first response so the model calls `write` instead.
///
/// Every frame is parsed, edited and re-serialized rather than string-patched: the
/// recording streams the arguments across five frames, and a textual splice leaves
/// malformed JSON that the provider rejects before the loop is reached. The frame
/// sequence, the finish reason and the usage frame stay as recorded; the tool's
/// name and its arguments are the only edited values, which is what `reason`
/// declares to [`MockProvider::authored_scenarios`].
fn write_tool_call_response(recorded: &str, target: &Path) -> MockResponse {
    let arguments = serde_json::json!({
        "filePath": target.to_string_lossy(),
        "content": WRITTEN_CONTENT,
        "intent": "prove the tool executes",
    })
    .to_string();
    let mut rewritten = String::new();
    for frame in recorded.split("\n\n") {
        let Some(payload) = frame.strip_prefix("data: ") else {
            continue;
        };
        if payload.trim() == "[DONE]" {
            rewritten.push_str("data: [DONE]\n\n");
            continue;
        }
        let mut chunk: serde_json::Value =
            serde_json::from_str(payload).expect("every recorded frame is JSON");
        let fragment = has_tool_call_fragment(&chunk);
        let named = chunk
            .pointer("/choices/0/delta/tool_calls/0/function/name")
            .is_some();
        if named {
            let call = chunk
                .pointer_mut("/choices/0/delta/tool_calls/0")
                .expect("the frame that names a function has the call");
            call["function"]["name"] = serde_json::Value::String("write".to_owned());
            call["function"]["arguments"] = serde_json::Value::String(arguments.clone());
        } else if fragment {
            continue;
        }
        rewritten.push_str("data: ");
        rewritten.push_str(&serde_json::to_string(&chunk).expect("the frame re-serializes"));
        rewritten.push_str("\n\n");
    }
    MockResponse::authored(
        200,
        "text/event-stream; charset=utf-8",
        rewritten,
        "no recording of a model calling this runtime's own tool names can exist \
         before this runtime has ever been driven by one; the frame sequence, \
         finish reason and usage frame are the recorded ones",
    )
}

fn plugin_tool_call_response(recorded: &str) -> MockResponse {
    rewrite_tool_call_response(
        recorded,
        ANTIGRAVITY_TOOL,
        serde_json::json!({
            "query": "production registry sentinel",
            "intent": "prove the real plugin tool executes"
        }),
        "the real antigravity tool is the acceptance target; only the function name and its arguments differ from the recorded framing",
    )
}

fn rewrite_tool_call_response(
    recorded: &str,
    tool: &str,
    arguments: serde_json::Value,
    reason: &str,
) -> MockResponse {
    let arguments = arguments.to_string();
    let mut rewritten = String::new();
    for frame in recorded.split("\n\n") {
        let Some(payload) = frame.strip_prefix("data: ") else {
            continue;
        };
        if payload.trim() == "[DONE]" {
            rewritten.push_str("data: [DONE]\n\n");
            continue;
        }
        let mut chunk: serde_json::Value =
            serde_json::from_str(payload).expect("every recorded frame is JSON");
        let fragment = has_tool_call_fragment(&chunk);
        let named = chunk
            .pointer("/choices/0/delta/tool_calls/0/function/name")
            .is_some();
        if named {
            let call = chunk
                .pointer_mut("/choices/0/delta/tool_calls/0")
                .expect("the frame that names a function has the call");
            call["function"]["name"] = serde_json::Value::String(tool.to_owned());
            call["function"]["arguments"] = serde_json::Value::String(arguments.clone());
        } else if fragment {
            continue;
        }
        rewritten.push_str("data: ");
        rewritten.push_str(&serde_json::to_string(&chunk).expect("the frame re-serializes"));
        rewritten.push_str("\n\n");
    }
    MockResponse::authored(200, "text/event-stream; charset=utf-8", rewritten, reason)
}

/// Whether a frame carries an arguments-only `tool_calls` fragment.
fn has_tool_call_fragment(chunk: &serde_json::Value) -> bool {
    chunk.pointer("/choices/0/delta/tool_calls").is_some()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_turn_executes_a_real_tool_and_the_side_effect_lands_on_disk() {
    let env = ScriptedEnv::new().expect("isolated environment");
    let target = env.project().join("written-by-the-tool.txt");
    let player = CassettePlayer::from_oracle(CASSETTE).expect("the recorded tool loop loads");
    let mut interactions = player.cassette().http_interactions();
    let first = interactions.next().expect("the tool-call interaction");
    let second = interactions.next().expect("the continuation interaction");
    let recorded_first = String::from_utf8(
        first
            .response
            .decoded_body(CASSETTE, 1)
            .expect("the recorded body decodes"),
    )
    .expect("the recorded body is UTF-8");

    let scenario = Scenario::new("write-tool-loop")
        .on_path("/v1/chat/completions")
        .from_oracle_cassette(TITLE_CASSETTE)
        .expect("the recorded text completion loads")
        .respond(write_tool_call_response(&recorded_first, &target))
        .respond(
            MockResponse::from_recorded(CASSETTE, 2, second).expect("the continuation decodes"),
        );
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");

    let output = run_prompt(&env, provider.base_url(), "Write the file.").await;
    let captured = provider.captured().await;
    provider.shutdown().await;

    assert!(
        output.status.success(),
        "run failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        captured.len(),
        REQUESTS_FOR_ONE_TOOL_TURN,
        "the turn did not continue after the tool result"
    );
    assert_eq!(
        std::fs::read_to_string(&target).ok().as_deref(),
        Some(WRITTEN_CONTENT),
        "the `write` tool did not run; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        captured[2]
            .json()
            .is_some_and(|body| has_tool_result(&body)),
        "the executed tool's result was not sent back to the model"
    );
    assert!(
        matches!(
            captured[1].served_origin.as_ref(),
            Some(ResponseOrigin::Authored { .. })
        ),
        "the rewritten first response must be reported as authored"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_plugin_tool_reaches_and_executes_through_the_production_registry() {
    let installed = Path::new("/config/.cache/opencode/packages")
        .join(ANTIGRAVITY_SPEC)
        .join("node_modules/opencode-antigravity-auth");
    if !installed.is_dir() {
        eprintln!(
            "SKIPPED a_real_plugin_tool_reaches_and_executes_through_the_production_registry: {} is absent",
            installed.display()
        );
        return;
    }
    let env = ScriptedEnv::new().expect("isolated environment");
    let player = CassettePlayer::from_oracle(CASSETTE).expect("the recorded tool loop loads");
    let mut interactions = player.cassette().http_interactions();
    let first = interactions.next().expect("the tool-call interaction");
    let second = interactions.next().expect("the continuation interaction");
    let recorded_first = String::from_utf8(
        first
            .response
            .decoded_body(CASSETTE, 1)
            .expect("the recorded body decodes"),
    )
    .expect("the recorded body is UTF-8");
    let scenario = Scenario::new("real-plugin-tool-loop")
        .on_path("/v1/chat/completions")
        .from_oracle_cassette(TITLE_CASSETTE)
        .expect("the recorded title completion loads")
        .respond(plugin_tool_call_response(&recorded_first))
        .respond(
            MockResponse::from_recorded(CASSETTE, 2, second).expect("the continuation decodes"),
        );
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");

    let output = run_plugin_prompt(
        &env,
        provider.base_url(),
        "Use the plugin search tool.",
        false,
    )
    .await;
    let captured = provider.captured().await;
    provider.shutdown().await;

    assert!(
        output.status.success(),
        "plugin-backed run failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(captured.len(), REQUESTS_FOR_ONE_TOOL_TURN);
    let offered = advertised_tools(&captured[1].json().expect("first turn request is JSON"));
    assert!(
        offered.iter().any(|name| name == ANTIGRAVITY_TOOL),
        "the real plugin's tool hook never reached the production registry: {offered:?}"
    );
    let continuation = captured[2]
        .json()
        .expect("the continuation request is JSON");
    let tool_result = continuation
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .and_then(|messages| {
            messages.iter().find(|message| {
                message.get("role").and_then(serde_json::Value::as_str) == Some("tool")
            })
        })
        .and_then(|message| message.get("content"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert!(
        tool_result.contains("Not authenticated with Antigravity"),
        "the model call did not execute antigravity's own google_search implementation; result={tool_result:?}, body={continuation:#}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_discovered_plugins_load_from_all_four_directories_through_the_real_binary() {
    let env = ScriptedEnv::new()
        .expect("isolated environment")
        .with_db(DbChoice::TempFile);
    let local = env.project().join(".opencode");
    let global = env.xdg_config().join("opencode");
    let plugin_files = [
        local.join("plugin/project-singular.js"),
        local.join("plugins/project-plural.js"),
        global.join("plugin/global-singular.js"),
        global.join("plugins/global-plural.js"),
    ];
    for plugin in &plugin_files {
        std::fs::create_dir_all(plugin.parent().expect("plugin parent"))
            .expect("create auto-plugin directory");
        std::fs::write(plugin, AUTO_DISCOVERY_PLUGIN).expect("write auto-discovered plugin");
    }
    let broken_config = env.project().join("broken-config");
    std::fs::create_dir_all(&broken_config).expect("create broken config directory");
    std::fs::write(broken_config.join("plugin"), "not a directory")
        .expect("create unreadable plugin directory stand-in");
    let load_log = env.project().join("auto-discovery.log");
    let scenario = Scenario::new("production-auto-discovery")
        .from_oracle_cassette(TITLE_CASSETTE)
        .expect("the recorded title completion loads")
        .from_oracle_cassette(TITLE_CASSETTE)
        .expect("the recorded turn completion loads");
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");

    let output = run_auto_discovery_prompt(&env, provider.base_url(), &load_log).await;
    let captured = provider.captured().await;
    provider.shutdown().await;

    assert!(
        output.status.success(),
        "auto-discovery run failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(captured.len(), 2, "title plus one ordinary turn");
    let loaded: std::collections::BTreeSet<_> = std::fs::read_to_string(&load_log)
        .expect("all four plugin factories must record their load")
        .lines()
        .map(str::to_owned)
        .collect();
    let expected: std::collections::BTreeSet<_> = plugin_files
        .iter()
        .map(|path| {
            path.file_name()
                .expect("plugin filename")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(loaded, expected, "every advertised directory must load");

    let stderr = String::from_utf8_lossy(&output.stderr);
    for plugin in &expected {
        assert!(
            stderr.contains(plugin),
            "DEBUG logs must name auto-discovered plugin {plugin}; stderr:\n{stderr}"
        );
    }
    assert!(
        stderr.contains("failed to auto-discover JavaScript plugins")
            && stderr.contains(&broken_config.join("plugin").display().to_string()),
        "a scan failure must identify the affected directory; stderr:\n{stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plugin_tool_is_hidden_by_the_same_permission_layer_as_builtins() {
    let installed = Path::new("/config/.cache/opencode/packages")
        .join(ANTIGRAVITY_SPEC)
        .join("node_modules/opencode-antigravity-auth");
    if !installed.is_dir() {
        eprintln!(
            "SKIPPED a_plugin_tool_is_hidden_by_the_same_permission_layer_as_builtins: {} is absent",
            installed.display()
        );
        return;
    }
    let env = ScriptedEnv::new().expect("isolated environment");
    let scenario = Scenario::new("denied-plugin-tool")
        .from_oracle_cassette(TITLE_CASSETTE)
        .expect("the recorded title completion loads")
        .from_oracle_cassette(TITLE_CASSETTE)
        .expect("the recorded turn completion loads");
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");

    let output = run_plugin_prompt(&env, provider.base_url(), "Answer without tools.", true).await;
    let captured = provider.captured().await;
    provider.shutdown().await;

    assert!(
        output.status.success(),
        "permission-gated run failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(captured.len(), 2, "title plus one ordinary turn");
    let offered = advertised_tools(&captured[1].json().expect("turn request is JSON"));
    assert!(
        offered.iter().any(|name| name == "bash"),
        "the control built-in must remain visible: {offered:?}"
    );
    assert!(
        !offered.iter().any(|name| name == ANTIGRAVITY_TOOL),
        "a user deny must hide a plugin tool exactly as it hides a built-in: {offered:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_plugin_lifecycle_hooks_run_through_the_real_dispatcher() {
    let env = ScriptedEnv::new().expect("isolated environment");
    let plugin = env.project().join("lifecycle-tool-plugin.mjs");
    let event_file = env.project().join("lifecycle-tool-plugin.events");
    let dispose_file = env.project().join("lifecycle-tool-plugin.dispose");
    std::fs::write(&plugin, LIFECYCLE_PLUGIN).expect("write lifecycle tool plugin");
    let player = CassettePlayer::from_oracle(CASSETTE).expect("the recorded tool loop loads");
    let mut interactions = player.cassette().http_interactions();
    let first = interactions.next().expect("the tool-call interaction");
    let second = interactions.next().expect("the continuation interaction");
    let recorded_first = String::from_utf8(
        first
            .response
            .decoded_body(CASSETTE, 1)
            .expect("the recorded body decodes"),
    )
    .expect("the recorded body is UTF-8");
    let scenario = Scenario::new("production-tool-lifecycle-hooks")
        .on_path("/v1/chat/completions")
        .from_oracle_cassette(TITLE_CASSETTE)
        .expect("the recorded title completion loads")
        .respond(rewrite_tool_call_response(
            &recorded_first,
            "lifecycle_tool",
            serde_json::json!({
                "value": "original",
                "intent": "prove plugin tool lifecycle hooks"
            }),
            "the plugin tool call is authored so the real dispatcher consumes the before, permission, and after hooks",
        ))
        .respond(
            MockResponse::from_recorded(CASSETTE, 2, second).expect("the continuation decodes"),
        );
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");

    let output = run_lifecycle_tool_prompt(
        &env,
        provider.base_url(),
        &plugin,
        &event_file,
        &dispose_file,
    )
    .await;
    let captured = provider.captured().await;
    provider.shutdown().await;

    assert!(
        output.status.success(),
        "tool lifecycle run failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(captured.len(), REQUESTS_FOR_ONE_TOOL_TURN);
    let continuation = captured[2]
        .json()
        .expect("the continuation request is JSON");
    let tool_result = continuation
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .and_then(|messages| {
            messages.iter().find(|message| {
                message.get("role").and_then(serde_json::Value::as_str) == Some("tool")
            })
        })
        .and_then(|message| message.get("content"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert_eq!(
        tool_result, "before-hook:after-hook",
        "tool.execute.before, permission.ask, and tool.execute.after must all affect the production result: {continuation:#}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_env_plugin_hook_reaches_the_real_shell_process() {
    let env = ScriptedEnv::new().expect("isolated environment");
    let plugin = env.project().join("lifecycle-shell-plugin.mjs");
    let event_file = env.project().join("lifecycle-shell-plugin.events");
    let dispose_file = env.project().join("lifecycle-shell-plugin.dispose");
    std::fs::write(&plugin, LIFECYCLE_PLUGIN).expect("write lifecycle shell plugin");
    let player = CassettePlayer::from_oracle(CASSETTE).expect("the recorded tool loop loads");
    let mut interactions = player.cassette().http_interactions();
    let first = interactions.next().expect("the tool-call interaction");
    let second = interactions.next().expect("the continuation interaction");
    let recorded_first = String::from_utf8(
        first
            .response
            .decoded_body(CASSETTE, 1)
            .expect("the recorded body decodes"),
    )
    .expect("the recorded body is UTF-8");
    let scenario = Scenario::new("production-shell-env-hook")
        .on_path("/v1/chat/completions")
        .from_oracle_cassette(TITLE_CASSETTE)
        .expect("the recorded title completion loads")
        .respond(rewrite_tool_call_response(
            &recorded_first,
            "bash",
            serde_json::json!({
                "command": "printf original",
                "intent": "prove shell.env reaches the child process"
            }),
            "the bash call is authored so shell.env can be observed in the real child process",
        ))
        .respond(
            MockResponse::from_recorded(CASSETTE, 2, second).expect("the continuation decodes"),
        );
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");

    let output = run_lifecycle_command_with_args(
        &env,
        provider.base_url(),
        &plugin,
        &event_file,
        &dispose_file,
        &["run", "--model", "test/test-model", "Use the shell once."],
    )
    .await;
    let captured = provider.captured().await;
    provider.shutdown().await;

    assert!(
        output.status.success(),
        "shell lifecycle run failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(captured.len(), REQUESTS_FOR_ONE_TOOL_TURN);
    let continuation = captured[2]
        .json()
        .expect("the continuation request is JSON");
    assert!(
        request_contains_text(&continuation, "shell-env-hook:after-hook"),
        "shell.env did not reach the spawned shell process: {continuation:#}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn noop_tool_definition_hook_preserves_real_schemas_and_stays_enabled() {
    let baseline_env = ScriptedEnv::new()
        .expect("isolated environment")
        .with_db(DbChoice::TempFile);
    let baseline_scenario = Scenario::new("tool-definition-baseline")
        .from_oracle_cassette(TITLE_CASSETTE)
        .expect("the recorded title completion loads")
        .from_oracle_cassette(TITLE_CASSETTE)
        .expect("the baseline turn completes");
    let baseline_provider = MockProvider::start(vec![baseline_scenario])
        .await
        .expect("baseline mock provider binds loopback");
    let baseline_output = run_prompt(&baseline_env, baseline_provider.base_url(), "hello").await;
    let baseline_captured = baseline_provider.captured().await;
    baseline_provider.shutdown().await;

    let env = ScriptedEnv::new()
        .expect("isolated plugin environment")
        .with_db(DbChoice::TempFile);
    let plugin = env.project().join("noop-tool-definition-plugin.mjs");
    let event_file = env.project().join("noop-tool-definition.events");
    std::fs::write(&plugin, NOOP_TOOL_DEFINITION_PLUGIN)
        .expect("write no-op tool.definition plugin");
    let scenario = Scenario::new("noop-tool-definition-round-trip")
        .from_oracle_cassette(TITLE_CASSETTE)
        .expect("the recorded title completion loads")
        .from_oracle_cassette(TITLE_CASSETTE)
        .expect("the turn completes with the plugin enabled");
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");

    let output =
        run_noop_tool_definition_prompt(&env, provider.base_url(), &plugin, &event_file).await;
    let captured = provider.captured().await;
    provider.shutdown().await;

    assert!(
        baseline_output.status.success(),
        "baseline run failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&baseline_output.stdout),
        String::from_utf8_lossy(&baseline_output.stderr)
    );
    assert!(
        output.status.success(),
        "the no-op hook must complete the CLI turn\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(baseline_captured.len(), 2, "baseline title plus turn");
    assert_eq!(captured.len(), 2, "plugin title plus turn");
    let baseline_turn = baseline_captured[1].json().expect("baseline turn is JSON");
    let plugin_turn = captured[1].json().expect("plugin turn is JSON");
    let baseline_tools =
        serde_json::to_vec(&baseline_turn["tools"]).expect("serialize baseline tool definitions");
    let plugin_tools =
        serde_json::to_vec(&plugin_turn["tools"]).expect("serialize plugin tool definitions");
    assert_eq!(
        plugin_tools, baseline_tools,
        "every real built-in schema must round-trip through the no-op hook byte-identically"
    );
    assert!(
        bridge_truncation_paths(&plugin_turn).is_empty(),
        "$truncated must never reach the provider request: {plugin_turn:#}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("disabled plugin"),
        "host data must not disable or blame the no-op plugin: {stderr}"
    );
    let calls = std::fs::read_to_string(&event_file).expect("read tool.definition calls");
    assert_eq!(
        calls.lines().collect::<Vec<_>>(),
        advertised_tools(&plugin_turn),
        "the plugin must remain enabled through every real tool definition: {calls:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failing_auth_loader_is_disabled_and_cli_run_completes_with_a_diagnostic() {
    let env = ScriptedEnv::new()
        .expect("isolated environment")
        .with_db(DbChoice::TempFile);
    let plugin = env.project().join("failing-auth-loader.mjs");
    std::fs::write(&plugin, FAILING_AUTH_LOADER_PLUGIN).expect("write failing auth loader");
    let scenario = Scenario::new("CLI auth loader failure is contained")
        .from_oracle_cassette(TITLE_CASSETTE)
        .expect("the recorded title completion loads")
        .from_oracle_cassette(TITLE_CASSETTE)
        .expect("the recorded turn completion loads");
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");

    let output = run_failing_auth_loader_prompt(&env, provider.base_url(), &plugin).await;
    let captured = provider.captured().await;
    provider.shutdown().await;

    assert!(
        output.status.success(),
        "the auth loader failure must disable only the plugin, not `run`\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(captured.len(), 2, "the CLI turn must reach the provider");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cli-failing-auth-loader")
            && stderr.contains("auth.loader")
            && stderr.contains("task173 CLI auth loader failure"),
        "the default CLI diagnostic must name plugin, hook, and cause: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_plugin_lifecycle_hooks_run_through_the_real_binary() {
    let env = ScriptedEnv::new()
        .expect("isolated environment")
        .with_db(DbChoice::TempFile);
    let plugin = env.project().join("lifecycle-plugin.mjs");
    let event_file = env.project().join("lifecycle-plugin.events");
    let dispose_file = env.project().join("lifecycle-plugin.dispose");
    std::fs::write(&plugin, LIFECYCLE_PLUGIN).expect("write lifecycle plugin");
    let scenario = Scenario::new("production-lifecycle-hooks")
        .from_oracle_cassette(TITLE_CASSETTE)
        .expect("the recorded title completion loads")
        .from_oracle_cassette(TITLE_CASSETTE)
        .expect("the recorded turn completion loads");
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");

    let output = run_lifecycle_command(
        &env,
        provider.base_url(),
        &plugin,
        &event_file,
        &dispose_file,
    )
    .await;
    let captured = provider.captured().await;
    provider.shutdown().await;

    let events = std::fs::read_to_string(&event_file).unwrap_or_default();
    let chat_message = events
        .lines()
        .find_map(|line| line.strip_prefix("chat.message="))
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("chat.message capture"))
        .expect("the production chat.message hook must record its payload");
    let input = &chat_message["input"];
    let message = &chat_message["message"];
    assert_eq!(
        message["id"], input["messageID"],
        "chat.message output.message.id must carry the live message id"
    );
    assert_eq!(
        message["sessionID"], input["sessionID"],
        "chat.message output.message.sessionID must carry the live session id"
    );
    assert_eq!(
        message["agent"], input["agent"],
        "chat.message output.message.agent must carry the live agent"
    );
    assert_eq!(
        message["model"], input["model"],
        "chat.message output.message.model must carry the live model"
    );
    assert!(
        output.status.success(),
        "lifecycle-plugin run failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(captured.len(), 2, "title plus one ordinary turn");
    let title = captured[0].json().expect("title request is JSON");
    assert_eq!(
        request_model(&title),
        Some("small-model"),
        "the internal title request did not consume experimental.provider.small_model: {title:#}"
    );
    let turn = captured[1].json().expect("turn request is JSON");
    let truncation_paths = bridge_truncation_paths(&turn);
    assert!(
        truncation_paths.is_empty(),
        "a no-op tool.definition hook silently committed bounded-encoder markers to the provider request: {truncation_paths:?}"
    );
    assert_eq!(
        request_model(&turn),
        Some("provider-hook-model"),
        "the provider resource hook did not replace the real catalog model: {turn:#}"
    );
    assert_eq!(
        turn.get("auth_hook_sentinel")
            .and_then(serde_json::Value::as_str),
        Some("auth-hook"),
        "the auth resource hook's provider options did not reach the real request: {turn:#}"
    );
    assert_eq!(
        turn.get("params_hook_sentinel")
            .and_then(serde_json::Value::as_str),
        Some("chat-params-hook"),
        "chat.params was not consumed by provider request preparation: {turn:#}"
    );
    assert_eq!(
        captured[1].header("x-chat-headers-hook"),
        Some("chat-headers-hook"),
        "chat.headers was not consumed by the real HTTP request"
    );
    assert!(
        request_contains_text(&turn, "config:raw arguments:command:chat:messages"),
        "the provider request did not consume config, command.execute.before, chat.message, and the messages transform in order: {turn:#}"
    );
    assert!(
        request_contains_text(&turn, "system-hook-sentinel"),
        "the provider request did not consume the system transform: {turn:#}"
    );
    assert!(
        advertised_tools(&turn)
            .iter()
            .any(|tool| tool == "lifecycle_tool"),
        "the tool resource hook did not enter the production registry: {turn:#}"
    );
    assert_eq!(
        advertised_tool_description(&turn, "lifecycle_tool"),
        Some("definition-hook-description"),
        "tool.definition did not mutate the provider-visible definition: {turn:#}"
    );
    assert!(
        events.lines().any(|event| event == "turn.completed"),
        "the event hook did not observe the production turn stream: {events:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&dispose_file).ok().as_deref(),
        Some("disposed\n"),
        "the run surface stopped the JavaScript host without dispatching dispose"
    );
    let database = env.xdg_data().join("scripted.db");
    let connection = oc_db::open_at(&database).expect("open lifecycle database");
    let mut statement = connection
        .prepare("SELECT data FROM part ORDER BY time_created, id")
        .expect("prepare lifecycle part query");
    let stored = statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query lifecycle parts")
        .collect::<Result<Vec<_>, _>>()
        .expect("read lifecycle parts")
        .join("\n");
    assert!(
        stored.contains(":text-complete-hook"),
        "experimental.text.complete did not alter the durable assistant text: {stored}"
    );
}

#[test]
fn tui_refuses_a_non_terminal_invocation_and_names_the_headless_surface() {
    // The one property of the boot path that is assertable without a TTY, and the
    // one that matters most: entering raw mode on a pipe would write escape
    // sequences into whatever is reading it with no way to type the exit key.
    let env = ScriptedEnv::new().expect("isolated environment");
    let output = std::process::Command::new(binary())
        .arg("tui")
        .current_dir(env.working_dir())
        .env_clear()
        .envs(env.env_vars())
        .output()
        .expect("launch opencode-rust tui");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("terminal"), "{stderr}");
    assert!(stderr.contains("run"), "{stderr}");
}

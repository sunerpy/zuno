use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use zuno_plugin::{
    HookInvocation, Plugin, PluginDiagnosticKind, PluginProcessSpec, TextCompleteInput,
    TextCompleteOutput, load_plugins_ordered,
};
use zuno_tool::{AllowAll, NeverInterrupted, ToolContext};

fn example_spec(id: &str) -> PluginProcessSpec {
    PluginProcessSpec::new(id, env!("CARGO_BIN_EXE_zuno-example-plugin"))
        .env("OC_EXAMPLE_PLUGIN_ID", id)
}

#[tokio::test]
async fn jsonrpc_example_plugin_tool_resolves_and_executes() {
    let load = load_plugins_ordered(vec![example_spec("happy")]).await;
    assert!(load.diagnostics().is_empty());
    load.validate_tool_names(["bash", "read", "write"])
        .expect("example does not shadow a built-in");
    let mut tools = zuno_plugin::PluginTools::new();
    load.hook_bus()
        .dispatch(HookInvocation::Tool { output: &mut tools })
        .await
        .expect("plugin tool resolution");
    let tool = tools.get("rust_echo").expect("example tool registered");
    let context = ToolContext::new(
        "session",
        "message",
        "call",
        "build",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    );

    let output = tool
        .execute(json!({ "text": "hello" }), context)
        .await
        .expect("remote tool executes");

    assert_eq!(output.title, "Rust echo");
    assert_eq!(output.output, "hello");
    load.shutdown().await;
}

#[tokio::test]
async fn jsonrpc_plugin_tool_cannot_silently_shadow_a_reserved_name() {
    let load = load_plugins_ordered(vec![example_spec("conflict")]).await;

    let error = load
        .validate_tool_names(["rust_echo"])
        .expect_err("reserved names must reject a plugin tool");

    assert_eq!(
        error.to_string(),
        "plugin tool `rust_echo` conflicts with a reserved tool name"
    );
    load.shutdown().await;
}

#[tokio::test]
async fn jsonrpc_hung_hook_is_disabled_and_the_turn_completes() {
    let timeout = Duration::from_millis(60);
    let load = load_plugins_ordered(vec![
        example_spec("slow")
            .env("OC_EXAMPLE_SLEEP_HOOK_MS", "500")
            .timeout(timeout),
    ])
    .await;
    let plugin = load.plugins().first().expect("slow plugin loaded");
    let bus = load.hook_bus();
    let mut output = TextCompleteOutput {
        text: "turn-completed".to_owned(),
    };

    tokio::time::timeout(
        Duration::from_secs(1),
        bus.dispatch(HookInvocation::TextComplete {
            input: &TextCompleteInput {
                session_id: "session",
                message_id: "message",
                part_id: "part",
            },
            output: &mut output,
        }),
    )
    .await
    .expect("the plugin deadline bounds the turn")
    .expect("a contained timeout does not fail the bus");

    assert_eq!(output.text, "turn-completed");
    assert!(!plugin.is_enabled());
    let diagnostics = load.diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].kind, PluginDiagnosticKind::TimedOut);
    assert_eq!(
        diagnostics[0].hook.as_deref(),
        Some("experimental.text.complete")
    );
    load.shutdown().await;
}

#[tokio::test]
async fn jsonrpc_startup_panic_is_reported_and_other_plugins_still_load() {
    let load = load_plugins_ordered(vec![
        example_spec("panic").env("OC_EXAMPLE_PANIC_STARTUP", "1"),
        example_spec("healthy"),
    ])
    .await;

    assert_eq!(load.plugins().len(), 1);
    assert_eq!(load.plugins()[0].manifest().id(), "healthy");
    let diagnostics = load.diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].plugin, "panic");
    assert_eq!(diagnostics[0].kind, PluginDiagnosticKind::FailedToLoad);
    load.shutdown().await;
}

#[tokio::test]
async fn jsonrpc_parallel_resolve_still_dispatches_in_configuration_order() {
    let gate = tempfile::tempdir().expect("startup gate");
    let gate_path = gate.path().to_string_lossy().into_owned();
    let gated = |id: &str, operation: &str| {
        example_spec(id)
            .env("OC_EXAMPLE_OPERATION", operation)
            .env("OC_EXAMPLE_STARTUP_GATE", &gate_path)
            .env("OC_EXAMPLE_GATE_COUNT", "2")
            .timeout(Duration::from_secs(3))
    };
    let load = load_plugins_ordered(vec![gated("first", "add"), gated("second", "multiply")]).await;
    assert!(load.diagnostics().is_empty());
    assert_eq!(load.plugins()[0].manifest().id(), "first");
    assert_eq!(load.plugins()[1].manifest().id(), "second");
    let mut output = TextCompleteOutput {
        text: "x".to_owned(),
    };

    load.hook_bus()
        .dispatch(HookInvocation::TextComplete {
            input: &TextCompleteInput {
                session_id: "session",
                message_id: "message",
                part_id: "part",
            },
            output: &mut output,
        })
        .await
        .expect("ordered remote dispatch");

    assert_eq!(output.text, "xAB");
    load.shutdown().await;
}

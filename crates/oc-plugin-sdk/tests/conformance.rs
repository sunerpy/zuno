use oc_plugin_sdk::{ConformanceSuite, HookCase, Plugin, ToolCase, ToolDefinition, ToolOutput};
use serde_json::json;

#[tokio::test]
async fn reusable_conformance_suite_checks_declared_tools_and_hooks() {
    let plugin = Plugin::new("third-party")
        .tool(
            ToolDefinition::new("echo", "Echo", json!({ "type": "object" })),
            |call| async move { Ok(ToolOutput::text("echo", call.arguments.to_string())) },
        )
        .expect("valid tool")
        .hook("experimental.text.complete", |mut call| async move {
            call.output["text"] = json!("checked");
            Ok(call)
        })
        .expect("valid hook");
    let suite = ConformanceSuite::new()
        .tool(ToolCase::new(
            "echo",
            json!({ "value": 1 }),
            ToolOutput::text("echo", r#"{"value":1}"#),
        ))
        .hook(HookCase::new(
            "experimental.text.complete",
            json!({}),
            json!({ "text": "before" }),
            json!({ "text": "checked" }),
        ));

    let report = suite.run(&plugin).await.expect("self-conformance");

    assert_eq!(report.hooks_checked, 1);
    assert_eq!(report.tools_checked, 1);
}

#[tokio::test]
async fn conformance_rejects_an_uncovered_declared_hook() {
    let plugin = Plugin::new("third-party")
        .hook("shell.env", |call| async move { Ok(call) })
        .expect("valid hook");

    let error = ConformanceSuite::new()
        .run(&plugin)
        .await
        .expect_err("declared hooks require exact cases");

    assert!(error.to_string().contains("every declared callback"));
}

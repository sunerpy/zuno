use std::path::Path;
use std::sync::Arc;

use serde_json::json;
use zuno_extension::{API_VERSION, DynamicState, ExtensionRegistry, Scope, lifecycle_tools};
use zuno_tool::{AllowAll, NeverInterrupted, ToolContext};

fn context(call: &str) -> ToolContext {
    ToolContext::new(
        "ses_extension",
        "msg_extension",
        call,
        "build",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
}

#[tokio::test]
async fn model_tools_define_then_activate_without_writing_static_state() {
    let scope = Scope::new(Path::new("/repo"));
    let registry = Arc::new(ExtensionRegistry::new());
    let tools = lifecycle_tools(scope.clone(), Vec::new(), Arc::clone(&registry));
    let define = tools
        .iter()
        .find(|tool| tool.id() == "extension_define")
        .expect("define tool");
    let run = tools
        .iter()
        .find(|tool| tool.id() == "extension_run")
        .expect("run tool");

    let defined = define
        .invoke(
            json!({
                "package": {
                    "apiVersion": API_VERSION,
                    "id": "temporary",
                    "description": "temporary workflow",
                    "workflows": {
                        "temporary": {
                            "description": "temporary",
                            "prompt": "Do the temporary workflow."
                        }
                    }
                }
            }),
            context("call_define"),
        )
        .await
        .expect("definition succeeds");

    assert!(defined.output.contains("inactive"));
    assert_eq!(
        registry.dynamic_statuses(&scope)[0].state,
        DynamicState::Defined
    );
    let before = registry.active_revision(&scope);

    let activated = run
        .invoke(json!({"id": "temporary"}), context("call_run"))
        .await
        .expect("activation succeeds");

    assert!(activated.output.contains("scheduled"));
    assert_eq!(registry.active_revision(&scope), before);
    assert!(registry.desired_revision(&scope) > before);
    assert_eq!(
        registry.dynamic_statuses(&scope)[0].state,
        DynamicState::PendingRun
    );
}

#[test]
fn define_tool_advertises_the_complete_typed_package_schema() {
    let tools = lifecycle_tools(
        Scope::new(Path::new("/repo")),
        Vec::new(),
        Arc::new(ExtensionRegistry::new()),
    );
    let schema = tools
        .iter()
        .find(|tool| tool.id() == "extension_define")
        .expect("define tool")
        .definition()
        .parameters;
    assert_eq!(
        schema["properties"]["package"]["properties"]["apiVersion"]["type"],
        "string"
    );
    assert!(
        schema["properties"]["package"]["properties"]["agents"].is_object(),
        "agent declarations are absent from the tool schema"
    );
    assert!(
        schema["properties"]["package"]["properties"]["workflows"].is_object(),
        "workflow declarations are absent from the tool schema"
    );
    assert!(
        schema["properties"]["package"]["properties"]["skills"].is_object(),
        "skill declarations are absent from the tool schema"
    );
}

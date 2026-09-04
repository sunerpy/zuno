use std::path::Path;
use std::sync::Arc;

use serde_json::json;
use zuno_extension::{
    API_VERSION, DynamicState, ExtensionRegistry, Package, Scope, StaticPackage, lifecycle_tools,
};
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

/// `extension_inspect` reports the manifest path a reader can act on, byte for byte.
///
/// `zuno_paths::wire_path` is `display_path(path).replace('\\', "/")` on every platform, so
/// rendering `source.manifest` with it reports a package under a directory the user named
/// `zuno\ws` as `.../zuno/ws/...`. On Linux and macOS `\` is an ordinary filename byte, so
/// that is a different — possibly existing — path, and the natural next step after
/// inspecting a package is to `read` or `grep` the manifest this field names.
#[tokio::test]
async fn inspect_reports_a_manifest_path_without_folding_its_separators() {
    const MANIFEST: &str = r"/tmp/zuno\ws/review-kit/extension.json";

    let package: Package = serde_json::from_value(json!({
        "apiVersion": API_VERSION,
        "id": "review-kit",
        "description": "review helpers",
        "workflows": {
            "outline": {"description": "outline a review", "prompt": "Outline the review."}
        }
    }))
    .expect("a valid package");
    let inspect = lifecycle_tools(
        Scope::new(Path::new("/repo")),
        vec![StaticPackage::new(package, MANIFEST).expect("the directory names the package")],
        Arc::new(ExtensionRegistry::new()),
    )
    .into_iter()
    .find(|tool| tool.id() == "extension_inspect")
    .expect("inspect tool");

    let output = inspect
        .invoke(json!({}), context("call_inspect"))
        .await
        .expect("inspection succeeds");
    let statuses: serde_json::Value =
        serde_json::from_str(&output.output).expect("inspection reports JSON");
    assert_eq!(
        statuses[0]["source"]["manifest"],
        json!(MANIFEST),
        "{}",
        output.output
    );
    assert!(
        !output.output.contains(r"zuno/ws"),
        "a folded separator names a different directory: {}",
        output.output
    );
}

/// A manifest path with no UTF-8 spelling is reported as unresolvable, never substituted.
///
/// `json!` cannot encode such a path at all and a lossy rendering would hand the model a
/// U+FFFD string it would then try to `read`. Inspection is read-only and still lists every
/// other package and every other field of this one, so the field fails closed rather than
/// the call failing.
#[cfg(unix)]
#[tokio::test]
async fn inspect_reports_an_unrepresentable_manifest_path_as_unresolvable() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;

    let manifest = PathBuf::from(OsStr::from_bytes(
        b"/tmp/zuno-\xff-ws/review-kit/extension.json",
    ));
    let package: Package = serde_json::from_value(json!({
        "apiVersion": API_VERSION,
        "id": "review-kit",
        "description": "review helpers",
        "workflows": {
            "outline": {"description": "outline a review", "prompt": "Outline the review."}
        }
    }))
    .expect("a valid package");
    let inspect = lifecycle_tools(
        Scope::new(Path::new("/repo")),
        vec![StaticPackage::new(package, manifest).expect("the directory names the package")],
        Arc::new(ExtensionRegistry::new()),
    )
    .into_iter()
    .find(|tool| tool.id() == "extension_inspect")
    .expect("inspect tool");

    let output = inspect
        .invoke(json!({}), context("call_inspect"))
        .await
        .expect("inspection still succeeds for every other field");
    let statuses: serde_json::Value =
        serde_json::from_str(&output.output).expect("inspection reports JSON");
    assert_eq!(statuses[0]["id"], json!("review-kit"), "{}", output.output);
    assert_eq!(
        statuses[0]["source"]["manifest"],
        serde_json::Value::Null,
        "{}",
        output.output
    );
    assert_eq!(
        statuses[0]["source"]["manifestUnrepresentable"],
        json!(true),
        "{}",
        output.output
    );
    assert!(
        !output.output.contains('\u{fffd}'),
        "a substituted path is not the path: {}",
        output.output
    );
}

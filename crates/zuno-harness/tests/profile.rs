use std::sync::Arc;
use zuno_engine::driver::{AgentDriver, DefaultAgentDriver};
use zuno_harness::{ToolContributions, ToolManifest, default_profile, profile, profile_with_tools};
use zuno_runtime::HarnessRuntime;
use zuno_tool::erase;
use zuno_tools::invalid::InvalidTool;
use zuno_tools::registry::BuiltinSlot;

#[tokio::test]
async fn the_default_profile_publishes_the_driver_and_complete_native_tool_manifest() {
    let runtime = HarnessRuntime::new("profile");
    runtime
        .activate_profile(default_profile())
        .await
        .expect("default profile activates");

    assert_eq!(
        runtime
            .service::<dyn AgentDriver>()
            .expect("agent driver")
            .name(),
        "default"
    );
    let tools = runtime.service::<ToolManifest>().expect("tool manifest");
    assert!(tools.contains(BuiltinSlot::Task));
    assert!(tools.contains(BuiltinSlot::Job));
    assert!(tools.contains(BuiltinSlot::Search));
    assert_eq!(tools.slots(), zuno_tools::registry::BUILTIN_ORDER);
    assert!(
        runtime
            .service::<ToolContributions>()
            .expect("tool contributions")
            .tools()
            .is_empty()
    );
}

#[tokio::test]
async fn a_custom_harness_selects_its_driver_and_tool_surface_without_loop_changes() {
    let runtime = HarnessRuntime::new("profile");
    let driver: Arc<dyn AgentDriver> = Arc::new(DefaultAgentDriver);
    runtime
        .activate_profile(profile(
            "benchmark",
            Arc::clone(&driver),
            ToolManifest::new([BuiltinSlot::Read, BuiltinSlot::Task]).expect("unique tool slots"),
        ))
        .await
        .expect("custom profile activates");

    let resolved_driver = runtime.service::<dyn AgentDriver>().expect("agent driver");
    assert!(Arc::ptr_eq(&resolved_driver, &driver));
    assert_eq!(
        runtime
            .service::<ToolManifest>()
            .expect("tool manifest")
            .slots(),
        [BuiltinSlot::Read, BuiltinSlot::Task]
    );
    assert_eq!(runtime.active_profile_id().as_deref(), Some("benchmark"));
}

#[tokio::test]
async fn a_profile_publishes_native_tool_contributions() {
    let runtime = HarnessRuntime::new("profile");
    let contributed = erase(InvalidTool::new());
    runtime
        .activate_profile(profile_with_tools(
            "custom-tools",
            Arc::new(DefaultAgentDriver),
            ToolManifest::new([]).expect("empty manifest"),
            ToolContributions::new([Arc::clone(&contributed)]).expect("unique tools"),
        ))
        .await
        .expect("custom profile activates");

    let tools = runtime
        .service::<ToolContributions>()
        .expect("tool contributions");
    assert_eq!(tools.tools().len(), 1);
    assert!(Arc::ptr_eq(&tools.tools()[0], &contributed));
}

#[test]
fn duplicate_tool_slots_fail_when_the_profile_is_built() {
    let error = ToolManifest::new([BuiltinSlot::Task, BuiltinSlot::Task])
        .expect_err("duplicate slots fail");

    assert_eq!(
        error.to_string(),
        "tool slot `task` is declared more than once"
    );
}

#[test]
fn duplicate_contributed_tool_ids_fail_before_mount() {
    let tool = erase(InvalidTool::new());
    let error =
        ToolContributions::new([Arc::clone(&tool), tool]).expect_err("duplicate tool ids fail");

    assert_eq!(
        error.to_string(),
        "tool `invalid` is contributed more than once"
    );
}

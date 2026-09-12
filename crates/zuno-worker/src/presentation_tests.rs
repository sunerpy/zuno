#[test]
fn an_old_checkpoint_may_gain_display_metadata_but_cannot_change_its_tool_contract() {
    let current = crate::tools::GatewayToolDispatcher::definition();
    let mut raw = serde_json::to_value(&current).unwrap();
    raw.as_object_mut().unwrap().remove("presentation");
    let old: zuno_tool::ToolDefinition = serde_json::from_value(raw).unwrap();
    assert!(super::definition_matches(&old, &current));
    let mut changed = old.clone();
    changed.parameters["additionalProperties"] = serde_json::json!(true);
    assert!(!super::definition_matches(&changed, &current));
    let mut changed = old;
    changed.presentation.action = zuno_types::activity::InvocationAction::Process;
    assert!(
        !super::definition_matches(&changed, &current),
        "explicitly recorded provenance must not be silently replaced"
    );
}

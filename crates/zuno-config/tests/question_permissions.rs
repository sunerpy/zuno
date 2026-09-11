use zuno_config::schema::permission::{PermissionConfig, permission_key};

#[test]
fn synchronous_and_deferred_questions_share_one_permission_boundary() {
    assert_eq!(permission_key("question"), "question");
    assert_eq!(permission_key("question_async"), "question");
    assert_eq!(permission_key("plan_exit"), "plan_exit");
}

#[test]
fn a_rule_under_the_async_alias_is_rejected_instead_of_being_silently_ineffective() {
    let error = serde_json::from_value::<PermissionConfig>(serde_json::json!({
        "mode":"allow_all", "rules":{"question_async":"allow"}
    }))
    .expect_err("use the governing permission key");
    assert!(error.to_string().contains("question"));
    serde_json::from_value::<PermissionConfig>(serde_json::json!({
        "mode":"allow_all", "rules":{"question":"deny"}
    }))
    .expect("explicit governing deny remains valid under allow_all");
}

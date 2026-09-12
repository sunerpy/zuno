//! Exercise the production resolver, not just the internal-model helper.
use super::*;

fn configured_fixture() -> ResumeFixture {
    let mut aaa = resume_provider("aaa", false);
    aaa["retry"] = json!({
        "max_attempts": 1, "recovery_window_ms": 1234,
        "initial_delay_ms": 10, "max_delay_ms": 20, "jitter_percent": 0
    });
    aaa["models"]["aaa-model"]["cost"] = json!({"input": 3.0, "output": 7.0});
    let mut zzz = resume_provider("zzz", true);
    zzz["retry"] = json!({"max_attempts": 4, "recovery_window_ms": 660000});
    zzz["models"]["zzz-model"]["cost"] = json!({"input": 5.0, "output": 9.0});
    ResumeFixture::new(resume_config(json!({"provider": {"aaa": aaa, "zzz": zzz}})))
}

fn resolved(plan: &TurnPlan) -> EngineModel {
    plan.resolver
        .resolve_model(&plan.provider_id, &plan.model_id)
        .expect("production model")
}

#[tokio::test]
async fn production_resolver_retains_provider_retry_and_cost() {
    let fixture = configured_fixture();
    let plan = fixture.resolve(fixture.options(SessionChoice::New)).await;
    let model = resolved(&plan);
    assert_eq!(model.retry_policy.max_attempts().get(), 1);
    assert_eq!(
        model.retry_policy.recovery_window(),
        std::time::Duration::from_millis(1234)
    );
    assert_eq!(model.cost.input, 3.0);
    assert_eq!(model.cost.output, 7.0);
    assert_eq!(model.catalog_provider_id, "aaa");
    assert_eq!(model.catalog_model_id, "aaa-model");
    assert!(plan.resolver.resolve_model("zzz", "zzz-model").is_none());
}

#[tokio::test]
async fn production_resolver_preserves_retry_after_restore_inheritance_and_switch() {
    let fixture = configured_fixture();
    fixture.seed(
        "ses_retry_restore",
        Some("build"),
        saved_reference("zzz", "zzz-model", Some("high")),
        1_787_381_100_000,
    );
    let session = SessionChoice::Existing("ses_retry_restore".to_owned());
    let mut restored = fixture.resolve(fixture.options(session.clone())).await;
    let before = resolved(&restored);
    assert_eq!(before.retry_policy.max_attempts().get(), 4);
    assert_eq!(
        before.retry_policy.recovery_window(),
        std::time::Duration::from_secs(660)
    );
    assert_eq!(before.cost.input, 5.0);
    assert_eq!(before.cost.output, 9.0);
    let inherited = serde_json::Map::from_iter([("reasoningEffort".to_owned(), json!("high"))]);
    restored.inherit_request_parameters(inherited.clone());
    let after = resolved(&restored);
    assert_eq!(after.retry_policy, before.retry_policy);
    assert_eq!(after.cost, before.cost);
    assert_eq!(after.reasoning_options, inherited);

    let mut switch = fixture.options(session);
    switch.model = Some("aaa/aaa-model".to_owned());
    let switched = resolved(&fixture.resolve(switch).await);
    assert_eq!(switched.retry_policy.max_attempts().get(), 1);
    assert_eq!(
        switched.retry_policy.recovery_window(),
        std::time::Duration::from_millis(1234)
    );
    assert_eq!(switched.cost.input, 3.0);
}

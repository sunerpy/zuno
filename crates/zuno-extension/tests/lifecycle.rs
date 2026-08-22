use std::path::Path;

use serde_json::json;
use zuno_extension::{
    API_VERSION, DynamicState, ExtensionRegistry, Package, Scope, StaticPackage, resolve_active,
};

fn package(id: &str) -> Package {
    serde_json::from_value(json!({
        "apiVersion": API_VERSION,
        "id": id,
        "description": "A review extension",
        "agents": {
            "reviewer": {
                "description": "Review a change",
                "mode": "subagent",
                "prompt": "Review trust boundaries."
            }
        },
        "workflows": {
            "review-change": {
                "description": "Review the current change",
                "prompt": "Review the current change and report concrete findings."
            }
        },
        "skills": [{
            "name": "review-guidance",
            "description": "Use when reviewing a change.",
            "content": "Inspect authorization and durable state."
        }]
    }))
    .expect("valid package")
}

#[test]
fn definitions_are_immutable_and_activation_is_explicit() {
    let registry = ExtensionRegistry::new();
    let scope = Scope::new(Path::new("/repo"));

    registry
        .define(&scope, package("review"))
        .expect("first definition succeeds");
    assert_eq!(
        registry.dynamic_statuses(&scope)[0].state,
        DynamicState::Defined
    );
    assert!(
        resolve_active(&scope, &[], &registry)
            .expect("inactive definitions are valid")
            .packages()
            .is_empty()
    );

    let duplicate = registry
        .define(&scope, package("review"))
        .expect_err("a package id is immutable inside one process");
    assert!(duplicate.to_string().contains("already defined"));

    let before = registry.composition_generation();
    registry
        .run(&scope, "review")
        .expect("definition activates");
    assert!(registry.composition_generation() > before);
    let active = resolve_active(&scope, &[], &registry).expect("active catalog");
    assert_eq!(active.packages().len(), 1);
    assert!(active.agents().contains_key("reviewer"));
    assert!(active.workflows().contains_key("review-change"));
    assert_eq!(active.skills()[0].name, "review-guidance");

    registry
        .stop(&scope, "review")
        .expect("active package stops");
    assert_eq!(
        registry.dynamic_statuses(&scope)[0].state,
        DynamicState::Stopped
    );
    assert!(
        resolve_active(&scope, &[], &registry)
            .expect("stopped definitions remain valid")
            .packages()
            .is_empty()
    );
}

#[test]
fn process_local_definitions_disappear_with_the_registry() {
    let scope = Scope::new(Path::new("/repo"));
    let first_process = ExtensionRegistry::new();
    first_process
        .define(&scope, package("temporary"))
        .expect("define");
    first_process.run(&scope, "temporary").expect("run");
    assert_eq!(
        resolve_active(&scope, &[], &first_process)
            .expect("active")
            .packages()
            .len(),
        1
    );

    let restarted_process = ExtensionRegistry::new();
    assert!(
        restarted_process.dynamic_statuses(&scope).is_empty(),
        "a fresh process registry must not recover an in-memory definition"
    );
    assert!(
        resolve_active(&scope, &[], &restarted_process)
            .expect("empty catalog")
            .packages()
            .is_empty()
    );
}

#[test]
fn removing_a_running_definition_changes_the_active_composition() {
    let scope = Scope::new(Path::new("/repo"));
    let registry = ExtensionRegistry::new();
    registry
        .define(&scope, package("temporary"))
        .expect("define");
    registry.run(&scope, "temporary").expect("run");
    let before = registry.composition_generation();

    registry
        .undefine(&scope, "temporary")
        .expect("running definition is removed");

    assert!(registry.composition_generation() > before);
    assert!(registry.dynamic_statuses(&scope).is_empty());
}

#[test]
fn a_failed_activation_never_commits_or_advances_the_composition() {
    let scope = Scope::new(Path::new("/repo"));
    let registry = ExtensionRegistry::new();
    registry
        .define(&scope, package("temporary"))
        .expect("define candidate");
    let static_package = StaticPackage::new(
        package("persistent"),
        Path::new("/repo/.zuno/extensions/persistent/extension.json"),
    )
    .expect("static provenance");
    let before = registry.composition_generation();

    let error = registry
        .run_with_static(&scope, "temporary", &[static_package])
        .expect_err("both packages claim the reviewer agent");

    assert!(error.to_string().contains("activation failed"));
    assert_eq!(registry.composition_generation(), before);
    assert_eq!(
        registry.dynamic_statuses(&scope)[0].state,
        DynamicState::Defined,
        "a failed validation exposed a partially active package"
    );
}

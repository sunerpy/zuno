use std::path::Path;
use std::sync::Arc;

use serde_json::json;
use zuno_extension::{
    API_VERSION, DynamicState, ExtensionRegistry, Package, Scope, StageOutcome, StaticPackage,
    resolve_active, resolve_desired,
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

fn pending(outcome: StageOutcome) -> zuno_extension::ExtensionTransaction {
    match outcome {
        StageOutcome::Pending(transaction) => transaction,
        StageOutcome::Unchanged { .. } => panic!("expected a pending composition"),
    }
}

fn commit(
    registry: &Arc<ExtensionRegistry>,
    transaction: zuno_extension::ExtensionTransaction,
) -> zuno_extension::CompositionLease {
    registry
        .begin_transition(&transaction)
        .expect("old composition is quiescent")
        .commit()
        .expect("prepared composition commits")
}

#[test]
fn activation_is_prepared_then_committed_after_the_consumer_rebuilds() {
    let registry = Arc::new(ExtensionRegistry::new());
    let scope = Scope::new(Path::new("/repo"));
    registry
        .define(&scope, package("review"))
        .expect("first definition succeeds");
    let active_before = registry.active_revision(&scope);

    let transaction = pending(
        registry
            .stage_run(&scope, "review", &[])
            .expect("activation prepares"),
    );

    assert_eq!(registry.active_revision(&scope), active_before);
    assert_eq!(registry.desired_revision(&scope), transaction.revision());
    assert_eq!(
        registry.dynamic_statuses(&scope)[0].state,
        DynamicState::PendingRun
    );
    assert!(
        resolve_active(&scope, &[], &registry)
            .expect("committed catalog")
            .packages()
            .is_empty(),
        "staging published a package before the consumer rebuilt"
    );
    let desired = resolve_desired(&scope, &[], &registry).expect("desired catalog");
    assert_eq!(desired.packages().len(), 1);
    assert!(desired.agents().contains_key("reviewer"));

    let _lease = commit(&registry, transaction.clone());

    assert_eq!(registry.active_revision(&scope), transaction.revision());
    assert_eq!(
        registry.dynamic_statuses(&scope)[0].state,
        DynamicState::Running
    );
    assert_eq!(
        resolve_active(&scope, &[], &registry)
            .expect("committed catalog")
            .packages()
            .len(),
        1
    );
}

#[test]
fn abort_restores_the_exact_committed_state() {
    let registry = Arc::new(ExtensionRegistry::new());
    let scope = Scope::new(Path::new("/repo"));
    registry
        .define(&scope, package("review"))
        .expect("definition");
    let run = pending(
        registry
            .stage_run(&scope, "review", &[])
            .expect("stage run"),
    );
    let _lease = commit(&registry, run);
    let active_revision = registry.active_revision(&scope);

    let stop = pending(registry.stage_stop(&scope, "review").expect("stage stop"));
    assert_eq!(
        registry.dynamic_statuses(&scope)[0].state,
        DynamicState::PendingStop
    );
    assert_eq!(
        resolve_active(&scope, &[], &registry)
            .expect("old composition remains active")
            .packages()
            .len(),
        1
    );
    assert!(
        resolve_desired(&scope, &[], &registry)
            .expect("candidate composition")
            .packages()
            .is_empty()
    );

    registry.abort(&stop).expect("candidate rebuild failed");

    assert_eq!(registry.active_revision(&scope), active_revision);
    assert_eq!(registry.desired_revision(&scope), active_revision);
    assert_eq!(
        registry.dynamic_statuses(&scope)[0].state,
        DynamicState::Running
    );
}

#[test]
fn running_undefine_is_transactional_but_inactive_undefine_is_immediate() {
    let registry = Arc::new(ExtensionRegistry::new());
    let scope = Scope::new(Path::new("/repo"));
    registry
        .define(&scope, package("running"))
        .expect("running definition");
    let run = pending(
        registry
            .stage_run(&scope, "running", &[])
            .expect("stage run"),
    );
    let lease = commit(&registry, run);
    registry
        .define(&scope, package("inactive"))
        .expect("inactive definition");

    assert!(matches!(
        registry
            .stage_undefine(&scope, "inactive")
            .expect("inactive removal"),
        StageOutcome::Unchanged { .. }
    ));
    let remove = pending(
        registry
            .stage_undefine(&scope, "running")
            .expect("running removal prepares"),
    );
    assert_eq!(
        registry.dynamic_statuses(&scope)[0].state,
        DynamicState::PendingUndefine
    );
    assert_eq!(
        resolve_active(&scope, &[], &registry)
            .expect("old active")
            .packages()
            .len(),
        1
    );
    assert!(
        resolve_desired(&scope, &[], &registry)
            .expect("desired removal")
            .packages()
            .is_empty()
    );

    drop(lease);
    let _lease = commit(&registry, remove);
    assert!(registry.dynamic_statuses(&scope).is_empty());
}

#[test]
fn a_failed_activation_never_creates_a_pending_transaction() {
    let scope = Scope::new(Path::new("/repo"));
    let registry = Arc::new(ExtensionRegistry::new());
    registry
        .define(&scope, package("temporary"))
        .expect("define candidate");
    let static_package = StaticPackage::new(
        package("persistent"),
        Path::new("/repo/.zuno/extensions/persistent/extension.json"),
    )
    .expect("static provenance");
    let before = registry.active_revision(&scope);

    let error = registry
        .stage_run(&scope, "temporary", &[static_package])
        .expect_err("both packages claim the reviewer agent");

    assert!(error.to_string().contains("activation failed"));
    assert_eq!(registry.active_revision(&scope), before);
    assert_eq!(registry.desired_revision(&scope), before);
    assert_eq!(
        registry.dynamic_statuses(&scope)[0].state,
        DynamicState::Defined
    );
}

#[test]
fn revisions_and_pending_mutations_are_scope_local() {
    let registry = Arc::new(ExtensionRegistry::new());
    let first = Scope::new(Path::new("/repo-one"));
    let second = Scope::new(Path::new("/repo-two"));
    registry
        .define(&first, package("first"))
        .expect("first definition");
    registry
        .define(&second, package("second"))
        .expect("second definition");
    let second_revision = registry.active_revision(&second);

    let transaction = pending(
        registry
            .stage_run(&first, "first", &[])
            .expect("stage first"),
    );

    assert_eq!(registry.active_revision(&second), second_revision);
    assert_eq!(registry.desired_revision(&second), second_revision);
    assert_eq!(
        registry.dynamic_statuses(&second)[0].state,
        DynamicState::Defined
    );
    let _lease = commit(&registry, transaction);
    assert_eq!(registry.active_revision(&second), second_revision);
}

#[test]
fn transition_reservation_requires_quiescence_and_blocks_new_consumers() {
    let registry = Arc::new(ExtensionRegistry::new());
    let scope = Scope::new(Path::new("/repo"));
    registry
        .define(&scope, package("review"))
        .expect("definition");
    let transaction = pending(
        registry
            .stage_run(&scope, "review", &[])
            .expect("stage run"),
    );
    let old = registry
        .acquire_active(&scope, registry.active_revision(&scope))
        .expect("old consumer");

    let error = registry
        .begin_transition(&transaction)
        .expect_err("live old consumer blocks the transition");
    assert!(error.to_string().contains("active consumer"));
    drop(old);

    let prepared = registry
        .begin_transition(&transaction)
        .expect("quiescent transition reserves");
    let error = registry
        .acquire_active(&scope, registry.active_revision(&scope))
        .expect_err("reservation blocks a late old consumer");
    assert!(error.to_string().contains("already reserved"));
    prepared.abort().expect("candidate never started");

    assert_eq!(
        registry.dynamic_statuses(&scope)[0].state,
        DynamicState::Defined
    );
    assert_eq!(
        registry.desired_revision(&scope),
        registry.active_revision(&scope)
    );
}

#[test]
fn an_abandoned_prepared_transition_marks_the_scope_uncertain() {
    let registry = Arc::new(ExtensionRegistry::new());
    let scope = Scope::new(Path::new("/repo"));
    registry
        .define(&scope, package("review"))
        .expect("definition");
    let transaction = pending(
        registry
            .stage_run(&scope, "review", &[])
            .expect("stage run"),
    );

    drop(
        registry
            .begin_transition(&transaction)
            .expect("transition reserves"),
    );

    assert_eq!(
        registry.dynamic_statuses(&scope)[0].state,
        DynamicState::Uncertain
    );
    assert!(registry.uncertainty(&scope).is_some());
    let error = registry
        .acquire_active(&scope, registry.active_revision(&scope))
        .expect_err("uncertain composition cannot accept a new consumer");
    assert!(error.to_string().contains("uncertain"));
}

#[test]
fn process_local_definitions_disappear_with_the_registry() {
    let scope = Scope::new(Path::new("/repo"));
    let first_process = Arc::new(ExtensionRegistry::new());
    first_process
        .define(&scope, package("temporary"))
        .expect("define");
    let transaction = pending(
        first_process
            .stage_run(&scope, "temporary", &[])
            .expect("stage run"),
    );
    let _lease = commit(&first_process, transaction);
    assert_eq!(
        resolve_active(&scope, &[], &first_process)
            .expect("active")
            .packages()
            .len(),
        1
    );

    let restarted_process = ExtensionRegistry::new();
    assert!(restarted_process.dynamic_statuses(&scope).is_empty());
    assert!(
        resolve_active(&scope, &[], &restarted_process)
            .expect("empty catalog")
            .packages()
            .is_empty()
    );
}

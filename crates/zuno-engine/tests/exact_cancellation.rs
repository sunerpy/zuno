use zuno_engine::interrupt::{HardInterruptReason, HardInterruptRequest, HardInterruptSource};
use zuno_engine::status::{ExpectedTurnError, SessionRunRegistry};

fn cancellation() -> HardInterruptRequest {
    HardInterruptRequest::new(
        HardInterruptSource::Acp,
        HardInterruptReason::RequestCancelled,
    )
}

#[test]
fn delayed_exact_cancel_does_not_cross_a_turn_boundary_on_the_same_lease() {
    let registry = SessionRunRegistry::new();
    let control = registry.control("session");
    let lease = registry.begin_turn("session").expect("lease");
    let t1 = lease.mark_turn_started("t1").expect("T1");
    let captured = control.cancel_target().expect("capture T1");
    drop(t1);
    let _t2 = lease.mark_turn_started("t2").expect("T2");

    assert!(!control.abort_target(&captured, cancellation()));
    assert!(matches!(
        control.abort_turn("t1", cancellation()),
        Err(ExpectedTurnError::Mismatch { actual_turn_id, .. }) if actual_turn_id == "t2"
    ));
    assert!(!lease.interrupt_signal().is_set());
    control.abort_turn("t2", cancellation()).expect("cancel T2");
    assert_eq!(lease.interrupt_request(), Some(cancellation()));
}

#[test]
fn delayed_exact_cancel_cannot_arm_the_next_lease_or_cancel_a_reused_turn_id() {
    let registry = SessionRunRegistry::new();
    let control = registry.control("session");
    let first = registry.begin_turn("session").expect("first lease");
    let t1 = first.mark_turn_started("t1").expect("T1");
    let captured = control.cancel_target().expect("capture first lease");
    drop(t1);
    drop(first);
    assert!(!control.abort_target(&captured, cancellation()));
    assert!(matches!(
        control.abort_turn("t1", cancellation()),
        Err(ExpectedTurnError::NoActiveTurn { .. })
    ));

    let second = registry.begin_turn("session").expect("next lease");
    let _t1 = second.mark_turn_started("t1").expect("reused external id");
    assert!(!control.abort_target(&captured, cancellation()));
    assert!(!second.interrupt_signal().is_set());
}

#[test]
fn exact_input_cancel_checks_the_binding_at_the_signal_boundary() {
    let registry = SessionRunRegistry::new();
    let control = registry.control("session");
    let lease = registry.begin_turn("session").expect("lease");
    let input1 = lease.mark_input_started("input1").expect("input1");
    let captured = control
        .cancel_target()
        .expect("capture input before turn starts");
    drop(input1);
    let input2 = lease.mark_input_started("input2").expect("input2");

    assert!(!control.abort_input("input1", cancellation()));
    assert!(!control.abort_target(&captured, cancellation()));
    assert!(!lease.interrupt_signal().is_set());
    assert!(control.abort_input("input2", cancellation()));
    assert_eq!(lease.interrupt_request(), Some(cancellation()));
    drop(input2);
    assert!(!control.abort_input("input2", cancellation()));
}

#[test]
fn clearing_an_old_identity_does_not_remove_a_new_binding() {
    let registry = SessionRunRegistry::new();
    let lease = registry.begin_turn("session").expect("lease");
    let old = lease.mark_input_started("old").expect("old input");
    let _new = lease.mark_input_started("new").expect("new input");
    drop(old);
    assert!(
        registry
            .control("session")
            .abort_input("new", cancellation())
    );
}

#[test]
fn cancellation_snapshots_are_scoped_to_their_registry_and_session() {
    let registry = SessionRunRegistry::new();
    let a = registry.begin_turn("a").expect("a");
    let _a = a.mark_turn_started("t1").expect("a turn");
    let target = registry.control("a").cancel_target().expect("capture a");
    let b = registry.begin_turn("b").expect("b");
    let _b = b.mark_turn_started("t1").expect("b turn");
    assert!(!registry.control("b").abort_target(&target, cancellation()));

    let other = SessionRunRegistry::new();
    let a2 = other.begin_turn("a").expect("a in another registry");
    let _a2 = a2.mark_turn_started("t1").expect("a2 turn");
    assert!(!other.control("a").abort_target(&target, cancellation()));
    assert!(!a.interrupt_signal().is_set());
    assert!(!b.interrupt_signal().is_set());
    assert!(!a2.interrupt_signal().is_set());
}

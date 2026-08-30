//! The exited-session retention cap, proven against real processes.
//!
//! The acceptance criterion is 30 sessions created, all exited, at most 25
//! retained. Counting is not enough: an implementation that evicts by *creation*
//! order also retains exactly 25 and retains the **wrong** 25, so
//! [`thirty_exited_sessions_retain_the_twenty_five_that_exited_last`] forces the
//! exit order to be the exact reverse of the creation order and asserts which ids
//! survived.
//!
//! Exits are sequenced by having each shell block on `read` from its own pty and
//! releasing it with a write. That is deterministic with no polling and no extra
//! processes — a gate file would need 30 shells spinning on `sleep`, which is both
//! slower and load-dependent.

mod common;

use zuno_pty::{EXITED_LIMIT, PtyId, PtyService, PtyServiceConfig, PtyStatus};

use common::{spawn_script, wait_for_exit, wait_for_exit_or_eviction};

/// Sessions created per pass. Five above the cap, so five must be evicted.
const CREATED: usize = 30;

/// A shell that exits with `code` only once something is written to it.
fn gated_script(code: usize) -> String {
    format!("read _gate; exit {code}")
}

fn release(service: &PtyService, id: &PtyId) {
    service
        .write(id, b"go\n")
        .expect("the gated session accepts input");
}

#[test]
fn thirty_exited_sessions_retain_at_most_twenty_five() {
    let service = PtyService::new(std::env::temp_dir());

    // Gated, so all 30 genuinely coexist before any exits. With children that
    // exit immediately the cap starts evicting while the loop is still creating,
    // and "30 sessions were created, then all exited" is never actually true —
    // observed as 26 of 30 present at the end of the create loop.
    let created: Vec<PtyId> = (0..CREATED)
        .map(|index| spawn_script(&service, &gated_script(index % 128)).id)
        .collect();
    assert_eq!(created.len(), CREATED);
    assert_eq!(
        service.list().len(),
        CREATED,
        "all {CREATED} sessions must coexist before any of them exits"
    );

    for id in &created {
        release(&service, id);
    }
    for id in &created {
        // Once more than the cap exits, the earliest exits are already evicted, so
        // demanding to observe every `exited` status would be racing the cap this
        // test exists to prove. Evicted implies exited: eviction is only ever
        // triggered from `record_exit`.
        wait_for_exit_or_eviction(&service, id);
    }

    // Eviction runs on the exiting session's own waiter thread, so the final
    // exit's eviction can still be in flight when its status flips.
    let settled = common::poll_until(|| service.list().len() <= EXITED_LIMIT);
    let retained = service.list();
    assert!(
        settled,
        "{} of {CREATED} sessions were still retained, above the cap of {EXITED_LIMIT}",
        retained.len()
    );
    assert_eq!(
        retained.len(),
        EXITED_LIMIT,
        "the cap must retain exactly {EXITED_LIMIT}, not fewer"
    );
    assert_eq!(service.retained_exited().len(), EXITED_LIMIT);

    for info in &retained {
        assert_eq!(info.status, PtyStatus::Exited);
        assert!(
            info.exit_code.is_some(),
            "a retained exited session must still answer for its exit code: {info:?}"
        );
        assert!(
            service.retained_output(&info.id).is_ok(),
            "a retained exited session must still answer for its output: {info:?}"
        );
    }

    let evicted: Vec<&PtyId> = created.iter().filter(|id| !service.contains(id)).collect();
    assert_eq!(
        evicted.len(),
        CREATED - EXITED_LIMIT,
        "exactly {} sessions must have been evicted",
        CREATED - EXITED_LIMIT
    );
    for id in evicted {
        assert!(
            service.get(id).is_err(),
            "an evicted session must be genuinely absent, not merely hidden"
        );
        assert!(service.retained_output(id).is_err());
    }
}

#[test]
fn thirty_exited_sessions_retain_the_twenty_five_that_exited_last() {
    let service = PtyService::new(std::env::temp_dir());

    let created: Vec<PtyId> = (0..CREATED)
        .map(|index| spawn_script(&service, &gated_script(index % 128)).id)
        .collect();
    assert_eq!(service.list().len(), CREATED);
    for id in &created {
        assert_eq!(
            service.get(id).expect("the session exists").status,
            PtyStatus::Running,
            "every session must still be gated before any release"
        );
    }

    // Release last-created first, so exit order is 29, 28, ... 0. The 25 most
    // recent exits are therefore ids 24 down to 0 — which includes the five
    // *oldest* by creation. A creation-ordered eviction would drop exactly those.
    let mut exit_order = Vec::with_capacity(CREATED);
    for index in (0..CREATED).rev() {
        release(&service, &created[index]);
        wait_for_exit(&service, &created[index]);
        exit_order.push(created[index].clone());
    }

    let settled = common::poll_until(|| service.retained_exited().len() <= EXITED_LIMIT);
    assert!(
        settled,
        "the cap was never reached: {} retained",
        service.retained_exited().len()
    );

    let expected_evicted = &exit_order[..CREATED - EXITED_LIMIT];
    let expected_retained = &exit_order[CREATED - EXITED_LIMIT..];

    assert_eq!(
        service.retained_exited(),
        expected_retained,
        "the retained queue must be exactly the {EXITED_LIMIT} most recent exits, \
         oldest exit first"
    );

    for id in expected_evicted {
        assert!(
            !service.contains(id),
            "{id} exited earliest and must have been evicted; eviction is not \
             following exit order"
        );
    }
    for id in expected_retained {
        assert!(
            service.contains(id),
            "{id} is a recent exit and must be retained"
        );
    }

    // Named explicitly rather than left to the set comparison: these five were
    // created first, so a creation-ordered implementation throws them away.
    for (index, id) in created.iter().take(CREATED - EXITED_LIMIT).enumerate() {
        assert!(
            service.contains(id),
            "session {index} ({id}) was created first but exited last, so it must \
             survive"
        );
    }
    // And these five were created last but exited first, so they must be the gone ones.
    for (offset, id) in created.iter().skip(EXITED_LIMIT).enumerate() {
        assert!(
            !service.contains(id),
            "session {} ({id}) was created last and exited first, so it must be gone",
            EXITED_LIMIT + offset
        );
    }
}

#[test]
fn removing_a_retained_session_frees_a_slot_for_a_later_exit() {
    let service =
        PtyService::with_config(PtyServiceConfig::new(std::env::temp_dir()).with_exited_limit(2));

    let first = spawn_script(&service, "exit 0").id;
    wait_for_exit(&service, &first);
    let second = spawn_script(&service, "exit 0").id;
    wait_for_exit(&service, &second);
    assert!(common::poll_until(|| service.retained_exited().len() == 2));

    service
        .remove(&first)
        .expect("the first session is retained");
    assert!(!service.contains(&first));
    assert_eq!(service.retained_exited(), vec![second.clone()]);

    let third = spawn_script(&service, "exit 0").id;
    wait_for_exit(&service, &third);
    assert!(
        common::poll_until(|| service.retained_exited().len() == 2),
        "retained {:?}",
        service.retained_exited()
    );
    assert!(
        service.contains(&second),
        "the removal freed a slot, so the second session must not have been evicted"
    );
    assert!(service.contains(&third));
}

#[test]
fn a_removed_session_is_gone_and_cannot_be_removed_twice() {
    let service = PtyService::new(std::env::temp_dir());
    let id = spawn_script(&service, &gated_script(0)).id;

    service
        .remove(&id)
        .expect("a running session can be removed");
    assert!(!service.contains(&id));
    assert!(
        service.remove(&id).is_err(),
        "a second removal must report not-found"
    );
    assert!(
        service.retained_exited().is_empty(),
        "a removal is not a retained exit"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn dropping_the_service_terminates_and_reaps_every_child() {
    let pids: Vec<u32> = {
        let service = PtyService::new(std::env::temp_dir());
        let running: Vec<zuno_pty::PtyInfo> = (0..4)
            .map(|_| spawn_script(&service, &gated_script(0)))
            .collect();
        for info in &running {
            assert_eq!(info.status, PtyStatus::Running);
            assert!(info.pid > 0, "a spawned child must report a pid: {info:?}");
            assert!(
                std::path::Path::new(&format!("/proc/{}", info.pid)).exists(),
                "the child {} was not running to begin with",
                info.pid
            );
        }
        running.into_iter().map(|info| info.pid).collect()
    };

    // `/proc/<pid>` survives while a child is a zombie, so its disappearance
    // proves both that the child was killed and that the waiter thread reaped it.
    for pid in pids {
        assert!(
            common::poll_until(|| !std::path::Path::new(&format!("/proc/{pid}")).exists()),
            "child {pid} outlived the service, or was never reaped"
        );
    }
}

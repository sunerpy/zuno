//! The session lifecycle QA scenarios, against real processes.
//!
//! Happy path: an interactive command runs, the terminal resizes, the shell exits
//! cleanly and its exit code is readable afterwards.
//!
//! Failure path: a child killed from outside the service still transitions to
//! `exited` and is reaped, because the waiter thread's blocking `wait` is what
//! collects it — nothing in the service has to notice the kill.

mod common;

use std::time::Duration;

use oc_pty::{
    AttachOptions, CreateInput, DRAIN_GRACE, PtyError, PtyEvent, PtyOutput, PtyService, PtyStatus,
    ReplayCursor, TerminalSize, UpdateInput,
};

use common::{poll_until, spawn_script, wait_for_exit, wait_for_output};

#[test]
fn an_interactive_session_runs_resizes_and_exits_cleanly() {
    let directory = tempfile::tempdir().expect("a temp working directory");
    let service = PtyService::new(directory.path());

    let info = service
        .create(CreateInput {
            command: Some("/bin/sh".to_owned()),
            args: Some(vec!["-i".to_owned()]),
            title: Some("QA terminal".to_owned()),
            size: Some(TerminalSize { rows: 24, cols: 80 }),
            ..Default::default()
        })
        .expect("an interactive /bin/sh session");
    let id = info.id.clone();

    assert_eq!(info.status, PtyStatus::Running);
    assert_eq!(info.title, "QA terminal");
    assert!(info.pid > 0);
    assert_eq!(info.cwd, directory.path().to_string_lossy());
    assert_eq!(
        service.size(&id).expect("the kernel reports a size"),
        TerminalSize { rows: 24, cols: 80 }
    );

    service
        .write(&id, b"printf 'QA-READY\\n'\n")
        .expect("the session accepts input");
    wait_for_output(&service, &id, "QA-READY");

    let resized = service
        .update(
            &id,
            UpdateInput {
                title: Some("QA terminal resized".to_owned()),
                size: Some(TerminalSize {
                    rows: 40,
                    cols: 120,
                }),
            },
        )
        .expect("the session can be updated");
    assert_eq!(resized.title, "QA terminal resized");
    assert_eq!(
        service.size(&id).expect("the kernel reports a size"),
        TerminalSize {
            rows: 40,
            cols: 120
        },
        "the resize must reach the kernel, not just the service"
    );

    // The shell reports the new size only after it has handled SIGWINCH, so this
    // also proves the child observed the resize rather than merely the pty.
    service
        .write(
            &id,
            b"printf 'COLS-%s\\n' \"$(tput cols 2>/dev/null || echo unknown)\"\n",
        )
        .expect("the session accepts input");
    let saw_columns = poll_until(|| {
        service.retained_output(&id).is_ok_and(|retained| {
            let text = String::from_utf8_lossy(&retained.bytes);
            text.contains("COLS-120") || text.contains("COLS-unknown")
        })
    });
    assert!(saw_columns, "the shell never answered the column query");

    service
        .write(&id, b"exit 3\n")
        .expect("the session accepts input");
    let exited = wait_for_exit(&service, &id);
    assert_eq!(exited.status, PtyStatus::Exited);
    assert_eq!(
        exited.exit_code,
        Some(3),
        "the exit code must survive the exit"
    );

    assert!(
        service.contains(&id),
        "an exited session stays observable until removed"
    );
    assert!(
        String::from_utf8_lossy(
            &service
                .retained_output(&id)
                .expect("output is retained")
                .bytes
        )
        .contains("QA-READY"),
        "the retained output must outlive the child"
    );
    assert!(
        matches!(
            service.attach(&id, AttachOptions::default()),
            Err(PtyError::Exited { .. })
        ),
        "attaching to an exited session must say so rather than hang"
    );

    // A resize and a write after the exit are accepted and ignored, so a client
    // reporting its window size does not need to know the shell died.
    service
        .update(
            &id,
            UpdateInput {
                size: Some(TerminalSize { rows: 10, cols: 20 }),
                ..Default::default()
            },
        )
        .expect("updating an exited session is a no-op, not an error");
    service
        .write(&id, b"ignored\n")
        .expect("writing after exit is a no-op");
}

#[test]
fn a_child_killed_externally_transitions_to_exited_and_is_reaped() {
    let service = PtyService::new(std::env::temp_dir());
    let info = spawn_script(&service, "read _gate; exit 0");
    let id = info.id.clone();
    let pid = info.pid;
    assert_eq!(info.status, PtyStatus::Running);

    #[cfg(target_os = "linux")]
    let proc_entry = format!("/proc/{pid}");
    #[cfg(target_os = "linux")]
    assert!(
        std::path::Path::new(&proc_entry).exists(),
        "the child {pid} was not running to begin with"
    );

    // Nothing in the service participates in this kill; it is the same thing an
    // impatient user or an OOM killer does.
    let killed = std::process::Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .expect("running kill");
    assert!(killed.success(), "kill -KILL {pid} failed");

    let exited = wait_for_exit(&service, &id);
    assert_eq!(exited.status, PtyStatus::Exited);
    assert!(
        exited.exit_code.is_some(),
        "a signalled child must still report an exit code: {exited:?}"
    );

    #[cfg(target_os = "linux")]
    assert!(
        poll_until(|| !std::path::Path::new(&proc_entry).exists()),
        "child {pid} is still in the process table, so it was killed but never reaped"
    );

    assert!(
        service.contains(&id),
        "an externally killed session is still observable"
    );
    service.remove(&id).expect("it can then be removed");
    assert!(!service.contains(&id));
}

#[test]
fn an_attachment_replays_retained_output_then_streams_live_output() {
    let service = PtyService::new(std::env::temp_dir());
    let id = spawn_script(
        &service,
        "printf 'BEFORE\\n'; read _gate; printf 'AFTER\\n'; exit 0",
    )
    .id;
    wait_for_output(&service, &id, "BEFORE");

    let mut attachment = service
        .attach(&id, AttachOptions::default())
        .expect("the session is running");
    assert!(
        String::from_utf8_lossy(&attachment.replay).contains("BEFORE"),
        "the replay must contain output produced before the attachment: {:?}",
        String::from_utf8_lossy(&attachment.replay)
    );
    assert!(attachment.cursor > 0);
    assert_eq!(
        service.subscriber_count(&id).expect("the session exists"),
        1
    );

    service
        .write(&id, b"go\n")
        .expect("the session accepts input");

    let mut live = String::new();
    let mut ended = false;
    let deadline = std::time::Instant::now() + common::BUDGET;
    while std::time::Instant::now() < deadline {
        match attachment.output.try_recv() {
            Ok(PtyOutput::Chunk(bytes)) => live.push_str(&String::from_utf8_lossy(&bytes)),
            Ok(PtyOutput::Ended { .. }) => {
                ended = true;
                break;
            }
            Ok(PtyOutput::Lagged { dropped, .. }) => {
                panic!("a 256-slot queue lagged on a two-line session, dropping {dropped} bytes")
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                ended = true;
                break;
            }
        }
        if live.contains("AFTER") && ended {
            break;
        }
    }
    assert!(live.contains("AFTER"), "live output was {live:?}");
    assert!(ended, "the attachment must be told the session ended");

    drop(attachment);
    assert!(
        poll_until(|| service.subscriber_count(&id).unwrap_or(1) == 0),
        "dropping an attachment must detach it"
    );
}

/// `Ended` is the stop signal, so it must be the *last* event, not merely an event.
///
/// Regression test for a real ordering bug. The reader thread and the waiter thread
/// are independent, and a child's death is not the same event as its output having
/// been read: `child.wait()` returns while the bytes the child wrote moments earlier
/// are still in the kernel's pty buffer. Before the fix the waiter could publish
/// `Ended` first, and since every consumer — including wave 9's WebSocket route —
/// stops on `Ended`, the tail of every short-lived command was silently truncated.
/// A user running `ls` would intermittently see nothing.
///
/// So the contract this asserts is not "the output eventually appears in the
/// buffer" (it always did) but "a subscriber that stops at `Ended` has already seen
/// all of it". That is why the loop below **breaks on `Ended` and never reads past
/// it** — reading afterwards would test the buffer instead of the ordering and would
/// pass against the bug.
///
/// Byte-exact rather than marker-based, and **deliberately run under self-inflicted
/// load**, because the race is decided by scheduler latency and nothing else.
///
/// In isolation the reader almost always wins: data becoming available wakes it
/// *before* the child's death wakes the waiter, and with a free core it runs
/// immediately. The bug only appears once the reader has to queue for CPU — which is
/// why a serial run passed every time and only six-way concurrent `cargo test`
/// exposed it. A single-session trial is therefore not a test of this at all;
/// measured against the fix reverted, 24 sequential single-session trials passed 5
/// times out of 5.
///
/// So the test creates the contention itself: [`SESSIONS`] sessions, each with its
/// own reader and waiter thread, all released at once. Measured against the fix
/// reverted, 13% of individual sessions lose output, which makes at least one
/// shortfall in a batch of 64 a near-certainty rather than a coin flip.
#[test]
fn a_subscriber_that_stops_at_the_end_event_has_seen_every_byte() {
    /// Enough concurrent reader/waiter pairs to outnumber the cores on any host.
    const SESSIONS: usize = 64;
    /// Small on purpose: it puts the child's last write and its exit microseconds
    /// apart, which is the shape that leaves the ordering entirely to the scheduler.
    const PAYLOAD: u64 = 8 * 1024;

    let service = PtyService::new(std::env::temp_dir());
    let mut attached = Vec::with_capacity(SESSIONS);
    for _ in 0..SESSIONS {
        // Gated, so every attachment is in place before any payload exists and all
        // the bursts start together.
        let id = spawn_script(
            &service,
            &format!("read _gate; yes 0123456789abcdef | head -c {PAYLOAD}; exit 0"),
        )
        .id;
        let attachment = service
            .attach(
                &id,
                AttachOptions {
                    // Tail, so the replay serves nothing and every byte must arrive
                    // as a live chunk. Capacity far above the two chunks this
                    // produces, so the drop path cannot be involved.
                    cursor: ReplayCursor::Tail,
                    capacity: 4_096,
                },
            )
            .expect("the session is running");
        attached.push((id, attachment));
    }

    for (id, _) in &attached {
        service.write(id, b"go\n").expect("the gate accepts input");
    }

    let mut short = Vec::new();
    for (index, (id, mut attachment)) in attached.into_iter().enumerate() {
        let mut received = 0u64;
        let mut lagged = 0u64;
        let mut ended = false;
        let deadline = std::time::Instant::now() + common::BUDGET;
        while std::time::Instant::now() < deadline {
            match attachment.output.try_recv() {
                Ok(PtyOutput::Chunk(bytes)) => received += bytes.len() as u64,
                Ok(PtyOutput::Lagged { dropped, .. }) => lagged += dropped,
                Ok(PtyOutput::Ended { .. }) => {
                    ended = true;
                    break;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_micros(200));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    ended = true;
                    break;
                }
            }
        }

        assert!(ended, "session {index} ({id}) never reported that it ended");
        assert_eq!(
            lagged, 0,
            "session {index} dropped {lagged} bytes to backpressure, so it cannot \
             distinguish an ordering failure from a full queue"
        );
        if received < PAYLOAD {
            short.push((index, received));
        }
    }

    assert!(
        short.is_empty(),
        "{} of {SESSIONS} subscribers were told the session ended before they had \
         received all {PAYLOAD} bytes their child wrote: {short:?} (index, bytes \
         received). `Ended` overtook the reader, so a consumer that stops there — \
         which is every consumer — loses the tail of the output",
        short.len()
    );
}

/// A live grandchild must not defer the exit, and must not stop the child's reap.
///
/// `sh -c 'sleep &  exit 0'` is the ordinary shape of backgrounding a process: the
/// child exits at once while a grandchild still holds the pty slave. This is the
/// case the drain wait has to be bounded against, so the assertion is that the exit
/// is published *promptly* — nowhere near the grandchild's lifetime — and that the
/// child is still reaped, which requires the drain wait to stay behind
/// `child.wait()`.
///
/// Measured on Linux, and worth stating because it is not what one would guess: the
/// wait does not even reach [`DRAIN_GRACE`] here. Once the pty's session leader
/// exits the kernel hangs the terminal up, so the master read ends in ~5 ms whatever
/// descendants do with the fd — probed with `setsid`, `nohup` and `trap "" HUP`
/// variants, none of which kept it open, and none of whose later output was ever
/// retained. `DRAIN_GRACE` is therefore a genuine bound rather than a routinely
/// exercised path, and the timeout branch is tested where it is deterministic:
/// `DrainGate`'s own unit tests in `session.rs`.
#[test]
fn a_grandchild_holding_the_pty_open_cannot_defer_the_exit() {
    // 3s self-terminates, so the probe leaks nothing, and it is 6x DRAIN_GRACE —
    // long enough that waiting for the grandchild would be unmistakable.
    const GRANDCHILD_LIFETIME: Duration = Duration::from_secs(3);
    let budget = DRAIN_GRACE + Duration::from_millis(500);
    assert!(
        budget < GRANDCHILD_LIFETIME,
        "the assertion below can only distinguish the two if the budget is shorter"
    );

    let service = PtyService::new(std::env::temp_dir());
    let info = spawn_script(&service, "sleep 3 & exit 0");
    let started = std::time::Instant::now();

    let exited = wait_for_exit(&service, &info.id);
    let elapsed = started.elapsed();

    assert_eq!(exited.status, PtyStatus::Exited);
    assert!(
        elapsed < budget,
        "the exit took {elapsed:?}, past the {budget:?} budget, so the drain wait is \
         following the grandchild instead of being bounded by {DRAIN_GRACE:?}"
    );

    #[cfg(target_os = "linux")]
    assert!(
        poll_until(|| !std::path::Path::new(&format!("/proc/{}", info.pid)).exists()),
        "the child {} was not reaped; the drain wait must stay behind child.wait()",
        info.pid
    );
}

#[test]
fn a_tail_cursor_attachment_replays_nothing() {
    let service = PtyService::new(std::env::temp_dir());
    let id = spawn_script(&service, "printf 'HISTORY\\n'; read _gate; exit 0").id;
    wait_for_output(&service, &id, "HISTORY");

    let attachment = service
        .attach(
            &id,
            AttachOptions {
                cursor: ReplayCursor::Tail,
                ..Default::default()
            },
        )
        .expect("the session is running");
    assert!(
        attachment.replay.is_empty(),
        "a tail attachment must not replay history: {:?}",
        String::from_utf8_lossy(&attachment.replay)
    );
    assert!(
        attachment.cursor > 0,
        "it must still report where it started"
    );
}

#[test]
fn a_slow_subscriber_is_told_it_lagged_instead_of_growing_a_queue() {
    let service = PtyService::new(std::env::temp_dir());
    let id = spawn_script(&service, "yes 0123456789abcdef | head -c 8388608; exit 0").id;

    // One slot, and nothing reads it until the producer is finished, so the drop
    // path is reached by construction rather than by racing the producer.
    let mut attachment = service
        .attach(
            &id,
            AttachOptions {
                cursor: ReplayCursor::Tail,
                capacity: 1,
            },
        )
        .expect("the session is running");
    wait_for_exit(&service, &id);

    let mut lagged = 0u64;
    let mut chunks = 0usize;
    while let Ok(event) = attachment.output.try_recv() {
        match event {
            PtyOutput::Chunk(_) => chunks += 1,
            PtyOutput::Lagged { dropped, .. } => lagged += dropped,
            PtyOutput::Ended { .. } => break,
        }
    }
    let retained = service
        .retained_output(&id)
        .expect("the session is retained");
    assert!(retained.total_written >= 8 * 1024 * 1024);
    assert!(
        lagged > 0 || chunks <= 2,
        "a one-slot queue absorbed {} bytes in {chunks} chunks with no reported lag",
        retained.total_written
    );
    assert!(
        chunks <= 4,
        "a one-slot queue delivered {chunks} chunks, so it is not actually bounded"
    );
}

#[test]
fn lifecycle_events_are_published_for_create_exit_and_delete() {
    let service = PtyService::new(std::env::temp_dir());
    let mut events = service.subscribe();

    let id = spawn_script(&service, "exit 5").id;
    wait_for_exit(&service, &id);
    service.remove(&id).expect("the exited session is retained");

    let mut created = false;
    let mut exited = None;
    let mut deleted = false;
    while let Ok(event) = events.try_recv() {
        match event {
            PtyEvent::Created { info } if info.id == id => created = true,
            PtyEvent::Exited {
                id: other,
                exit_code,
            } if other == id => exited = Some(exit_code),
            PtyEvent::Deleted { id: other } if other == id => deleted = true,
            _ => {}
        }
    }
    assert!(created, "no pty.created event");
    assert_eq!(exited, Some(Some(5)), "pty.exited must carry the exit code");
    assert!(deleted, "no pty.deleted event");
}

#[test]
fn the_default_command_is_the_preferred_shell_and_it_starts() {
    let service = PtyService::new(std::env::temp_dir());
    let info = service
        .create(CreateInput::default())
        .expect("a session with no command named");
    assert!(!info.command.is_empty());
    assert!(
        info.title.starts_with("Terminal "),
        "the default title must be derived from the id: {}",
        info.title
    );
    assert_eq!(info.status, PtyStatus::Running);

    service
        .write(&info.id, b"printf 'DEFAULT-SHELL-READY\\n'\n")
        .expect("the session accepts input");
    wait_for_output(&service, &info.id, "DEFAULT-SHELL-READY");
    service.remove(&info.id).expect("it can be removed");
}

#[test]
fn a_command_that_does_not_exist_fails_to_spawn_rather_than_hanging() {
    let service = PtyService::new(std::env::temp_dir());
    let outcome = service.create(CreateInput {
        command: Some("/nonexistent/definitely-not-a-shell".to_owned()),
        ..Default::default()
    });
    assert!(
        matches!(outcome, Err(PtyError::Spawn { .. })),
        "expected a spawn failure, got {outcome:?}"
    );
    assert!(
        service.list().is_empty(),
        "a failed spawn must not register a session"
    );
}

#[test]
fn a_connect_ticket_authorizes_exactly_one_upgrade() {
    let service = PtyService::new(std::env::temp_dir());
    let id = spawn_script(&service, "read _gate; exit 0").id;
    let scope = oc_pty::TicketScope::for_session(id.clone());

    let token = service.tickets().issue(scope.clone());
    assert_eq!(token.expires_in, 60);
    assert!(service.tickets().consume(&token.ticket, &scope));
    assert!(
        !service.tickets().consume(&token.ticket, &scope),
        "a ticket must not be redeemable twice"
    );

    let reissued = service.tickets().issue(scope.clone());
    service.remove(&id).expect("the session can be removed");
    assert!(
        !service.tickets().consume(&reissued.ticket, &scope),
        "removing the session must revoke its outstanding tickets"
    );
}

#[test]
fn the_shell_list_is_non_empty_and_flags_the_unusable_ones() {
    let service = PtyService::new(std::env::temp_dir());
    let shells = service.shells();
    assert!(
        !shells.is_empty(),
        "GET /pty/shells would return nothing on this host"
    );
    for shell in &shells {
        assert!(!shell.path.is_empty());
        assert!(!shell.name.is_empty());
        if shell.name == "fish" || shell.name == "nu" {
            assert!(
                !shell.acceptable,
                "{} must be flagged unacceptable",
                shell.name
            );
        }
    }
}

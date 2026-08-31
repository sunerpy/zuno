#![cfg(windows)]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use zuno_pty::{CreateInput, PtyError, PtyEvent, PtyService, PtyStatus};

const BUDGET: Duration = Duration::from_secs(20);
const EXIT_EVENT_GRACE: Duration = Duration::from_secs(1);
const EXIT_EVENT_WORKERS: usize = 8;
const EXIT_EVENT_ITERATIONS: usize = 8;

#[derive(Default)]
struct ExitEventCounts {
    prompt: AtomicUsize,
    late: AtomicUsize,
    lost: AtomicUsize,
    deleted_first: AtomicUsize,
}

fn poll_until(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + BUDGET;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn command_line(shell: &str, marker: &str, exit_code: u32) -> Vec<u8> {
    let name = Path::new(shell)
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or(shell)
        .to_ascii_lowercase();
    if name == "pwsh" || name == "powershell" {
        format!("Write-Output '{marker}'; exit {exit_code}\r\n").into_bytes()
    } else if name == "bash" || name == "sh" {
        format!("printf '{marker}\\n'; exit {exit_code}\n").into_bytes()
    } else {
        format!("echo {marker} & exit /b {exit_code}\r\n").into_bytes()
    }
}

fn wait_for_marker_and_exit(service: &PtyService, id: &zuno_pty::PtyId, marker: &str) {
    let settled = poll_until(|| {
        let info = service.get(id).ok();
        let output = service.retained_output(id).ok();
        info.is_some_and(|info| info.status == PtyStatus::Exited)
            && output.is_some_and(|output| String::from_utf8_lossy(&output.bytes).contains(marker))
    });
    assert!(
        settled,
        "Windows PTY did not emit {marker:?} and exit; info={:?}, output={:?}",
        service.get(id),
        service
            .retained_output(id)
            .map(|output| String::from_utf8_lossy(&output.bytes).into_owned())
    );
}

#[test]
fn the_default_windows_shell_starts_emits_output_and_exits() {
    let service = PtyService::new(std::env::temp_dir());
    let mut events = service.subscribe();
    let info = service
        .create(CreateInput::default())
        .expect("the preferred Windows shell starts in a ConPTY");
    assert_eq!(info.status, PtyStatus::Running);
    assert!(info.pid > 0);
    assert!(!info.command.is_empty());

    service
        .write(
            &info.id,
            &command_line(&info.command, "WINDOWS-PTY-READY", 7),
        )
        .expect("the Windows PTY accepts input");
    wait_for_marker_and_exit(&service, &info.id, "WINDOWS-PTY-READY");

    let exited = service.get(&info.id).expect("exited session is retained");
    assert_eq!(exited.exit_code, Some(7));
    let mut saw_created = false;
    let mut saw_exited = false;
    while let Ok(event) = events.try_recv() {
        match event {
            PtyEvent::Created { info: created } if created.id == info.id => saw_created = true,
            PtyEvent::Exited { id, exit_code } if id == info.id => {
                assert_eq!(exit_code, Some(7));
                saw_exited = true;
            }
            _ => {}
        }
    }
    assert!(saw_created, "no pty.created event for the Windows session");
    assert!(saw_exited, "no pty.exited event for the Windows session");
}

#[test]
fn explicit_cmd_exercises_native_conpty_lifecycle() {
    let service = PtyService::new(std::env::temp_dir());
    let info = service
        .create(CreateInput {
            command: Some("cmd.exe".to_owned()),
            args: Some(vec!["/Q".to_owned()]),
            ..Default::default()
        })
        .expect("cmd.exe starts in a ConPTY");
    service
        .write(&info.id, b"echo CMD-CONPTY-READY & exit /b 3\r\n")
        .expect("cmd.exe accepts input");
    wait_for_marker_and_exit(&service, &info.id, "CMD-CONPTY-READY");
    assert_eq!(
        service
            .get(&info.id)
            .expect("retained cmd session")
            .exit_code,
        Some(3)
    );
}

#[test]
fn a_missing_windows_command_fails_without_registering_a_session() {
    let service = PtyService::new(std::env::temp_dir());
    let outcome = service.create(CreateInput {
        command: Some(r"Z:\zuno-does-not-exist\missing-shell.exe".to_owned()),
        ..Default::default()
    });
    assert!(
        matches!(outcome, Err(PtyError::Spawn { .. })),
        "expected a spawn failure, got {outcome:?}"
    );
    assert!(service.list().is_empty());
}

fn observe_one_native_exit(counts: &ExitEventCounts) {
    let service = PtyService::new(std::env::temp_dir());
    let mut events = service.subscribe();
    let info = service
        .create(CreateInput {
            command: Some("cmd.exe".to_owned()),
            args: Some(vec![
                "/Q".to_owned(),
                "/D".to_owned(),
                "/C".to_owned(),
                "exit /b 5".to_owned(),
            ]),
            ..Default::default()
        })
        .expect("the native Windows exit command starts");
    let id = info.id;

    assert!(
        poll_until(|| {
            service
                .get(&id)
                .is_ok_and(|current| current.status == PtyStatus::Exited)
        }),
        "native Windows session did not exit; last observed {:?}",
        service.get(&id)
    );
    service
        .remove(&id)
        .expect("the exited Windows session is retained");

    let mut exit_code = None;
    let mut order = Vec::new();
    while let Ok(event) = events.try_recv() {
        match event {
            PtyEvent::Created { info } if info.id == id => order.push("created"),
            PtyEvent::Exited {
                id: other,
                exit_code: code,
            } if other == id => {
                exit_code = Some(code);
                order.push("exited");
            }
            PtyEvent::Deleted { id: other } if other == id => order.push("deleted"),
            _ => {}
        }
    }

    if let (Some(deleted), Some(exited)) = (
        order.iter().position(|entry| *entry == "deleted"),
        order.iter().position(|entry| *entry == "exited"),
    ) && deleted < exited
    {
        counts.deleted_first.fetch_add(1, Ordering::Relaxed);
    }

    if exit_code == Some(Some(5)) {
        counts.prompt.fetch_add(1, Ordering::Relaxed);
        return;
    }

    let deadline = Instant::now() + EXIT_EVENT_GRACE;
    while Instant::now() < deadline {
        match events.try_recv() {
            Ok(PtyEvent::Exited { id: other, .. }) if other == id => {
                counts.late.fetch_add(1, Ordering::Relaxed);
                return;
            }
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
    counts.lost.fetch_add(1, Ordering::Relaxed);
}

#[test]
fn an_exit_event_is_visible_before_a_removed_windows_session_disappears() {
    let counts = Arc::new(ExitEventCounts::default());
    let workers: Vec<_> = (0..EXIT_EVENT_WORKERS)
        .map(|_| {
            let counts = Arc::clone(&counts);
            std::thread::spawn(move || {
                for _ in 0..EXIT_EVENT_ITERATIONS {
                    observe_one_native_exit(&counts);
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("Windows exit-event worker");
    }

    let total = EXIT_EVENT_WORKERS * EXIT_EVENT_ITERATIONS;
    let prompt = counts.prompt.load(Ordering::Relaxed);
    let late = counts.late.load(Ordering::Relaxed);
    let lost = counts.lost.load(Ordering::Relaxed);
    let deleted_first = counts.deleted_first.load(Ordering::Relaxed);
    assert_eq!(
        lost, 0,
        "{lost} of {total} native Windows exit events were never delivered \
         (prompt={prompt} late={late})"
    );
    assert_eq!(
        late, 0,
        "{late} of {total} native Windows exit events arrived only after status was visible"
    );
    assert_eq!(
        deleted_first, 0,
        "{deleted_first} of {total} native Windows sessions reported Deleted before Exited"
    );
    assert_eq!(prompt, total, "every native Windows exit must be prompt");
}

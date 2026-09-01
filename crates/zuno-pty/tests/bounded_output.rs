#![cfg(unix)]

//! The per-session output bound, measured against a real 100 MB producer.
//!
//! [`buffer`]'s own unit test proves the ring is bounded when driven directly. This
//! file proves the *wiring* is bounded: a live child writing 100 MB through a real
//! pty, read by the session's reader thread, still leaves a 2 MiB buffer. That is
//! the failure mode this project exists to fix, so it is asserted end to end and
//! not only at the data structure.

mod common;

use zuno_pty::{BUFFER_LIMIT, PtyService, PtyServiceConfig, ReplayCursor};

use common::{spawn_script, wait_for_exit, wait_for_output, wait_for_total_written};

/// Output the producer must emit before the assertion is meaningful.
const HUNDRED_MEGABYTES: u64 = 100 * 1024 * 1024;

fn rss_kib() -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("VmRSS:"))
                .and_then(|value| value.split_whitespace().next()?.parse().ok())
        })
}

#[test]
fn a_session_producing_one_hundred_megabytes_holds_a_bounded_buffer() {
    let service = PtyService::new(std::env::temp_dir());
    let before = rss_kib();

    // `yes` is the cheapest unbounded producer available, and `head -c` bounds it
    // at exactly 100 MiB so the test terminates without relying on a kill.
    let session = spawn_script(
        &service,
        &format!("yes 0123456789abcdefghijklmnopqrstuvwxyz | head -c {HUNDRED_MEGABYTES}"),
    );
    let id = session.id.clone();

    wait_for_total_written(&service, &id, HUNDRED_MEGABYTES);
    wait_for_exit(&service, &id);

    let retained = service
        .retained_output(&id)
        .expect("the exited session is still retained");
    let reserved = service
        .reserved_bytes(&id)
        .expect("the session is retained");

    assert!(
        retained.total_written >= HUNDRED_MEGABYTES,
        "the producer emitted only {} bytes, so the bound was never exercised",
        retained.total_written
    );
    assert_eq!(retained.limit, BUFFER_LIMIT);
    assert!(
        retained.bytes.len() <= BUFFER_LIMIT,
        "retained {} bytes against a {BUFFER_LIMIT}-byte ceiling after {} bytes of output",
        retained.bytes.len(),
        retained.total_written
    );
    assert!(
        reserved <= BUFFER_LIMIT,
        "the ring reserved {reserved} bytes against a {BUFFER_LIMIT}-byte ceiling"
    );
    assert_eq!(
        retained.discarded,
        retained.total_written - retained.bytes.len() as u64,
        "every byte is either retained or accounted for as discarded"
    );
    assert_eq!(
        retained.end_cursor - retained.start_cursor,
        retained.bytes.len() as u64,
        "the cursor span must equal the retained length"
    );

    println!(
        "100 MB PTY output: total_written={} retained={} reserved={} discarded={} limit={}",
        retained.total_written,
        retained.bytes.len(),
        reserved,
        retained.discarded,
        retained.limit
    );

    // Corroboration only; the ceiling above is the proof. RSS is page-granular and
    // includes the 100 MB of transient read buffers the allocator may not have
    // returned to the OS, so the allowance is deliberately loose.
    if let (Some(before), Some(after)) = (before, rss_kib()) {
        let delta = after.saturating_sub(before);
        println!("100 MB PTY output: process RSS delta {delta} KiB");
        assert!(
            delta <= 64 * 1024,
            "RSS grew {delta} KiB while streaming {} bytes through a \
             {BUFFER_LIMIT}-byte buffer",
            retained.total_written
        );
    }
}

#[test]
fn a_small_buffer_keeps_the_tail_of_a_much_larger_stream() {
    // The same property at a size that runs in milliseconds, so a regression is
    // caught by the fast path of the suite as well as by the 100 MB case.
    let service = PtyService::with_config(
        PtyServiceConfig::new(std::env::temp_dir()).with_buffer_limit(4_096),
    );
    let session = spawn_script(
        &service,
        "yes abcdefghij | head -c 1048576; printf 'DONE-MARKER'",
    );
    let id = session.id;

    wait_for_output(&service, &id, "DONE-MARKER");
    wait_for_exit(&service, &id);

    let retained = service
        .retained_output(&id)
        .expect("the session is retained");
    assert!(
        retained.bytes.len() <= 4_096,
        "retained {}",
        retained.bytes.len()
    );
    assert!(retained.total_written > 1_000_000);
    // A PTY backend may append a small transport trailer while closing. The
    // product contract is that the child's newest marker remains in the bounded
    // tail, not that it is literally the final byte produced by the terminal.
    assert!(
        String::from_utf8_lossy(&retained.bytes).contains("DONE-MARKER"),
        "the newest marker must remain in the retained tail: {:?}",
        retained.bytes
    );
    assert!(
        retained.discarded > 1_000_000,
        "discarded only {} bytes",
        retained.discarded
    );
}

#[test]
fn a_replay_of_a_truncated_buffer_reports_the_cursor_it_actually_served() {
    let service = PtyService::with_config(
        PtyServiceConfig::new(std::env::temp_dir()).with_buffer_limit(2_048),
    );
    let session = spawn_script(&service, "yes 0123456789 | head -c 262144; printf 'TAIL'");
    let id = session.id;

    wait_for_output(&service, &id, "TAIL");
    let retained = service
        .retained_output(&id)
        .expect("the session is retained");
    assert!(
        retained.start_cursor > 0,
        "nothing was discarded, so nothing was truncated"
    );

    // Asking from cursor 0 cannot be honoured; the reply must say so through its
    // own cursor rather than by silently returning a shorter buffer.
    let full = service
        .retained_output(&id)
        .expect("the session is retained")
        .bytes;
    let attachment = service.attach(
        &id,
        zuno_pty::AttachOptions {
            cursor: ReplayCursor::From(0),
            ..Default::default()
        },
    );
    match attachment {
        Ok(attachment) => {
            assert!(attachment.replay.len() <= 2_048);
            assert!(
                attachment.cursor >= retained.end_cursor,
                "the served cursor {} predates the snapshot {}",
                attachment.cursor,
                retained.end_cursor
            );
        }
        // The producer is short-lived; losing the race to attach is not a failure of
        // the bound, and the snapshot above already proved truncation happened.
        Err(zuno_pty::PtyError::Exited { .. }) => {
            assert!(full.len() <= 2_048);
        }
        Err(error) => panic!("attach failed for an unexpected reason: {error}"),
    }
}

#[test]
fn multibyte_output_survives_truncation_without_replacement_characters() {
    // 3-byte code points against a limit that is not a multiple of 3, so the cut
    // lands mid-character unless the head is realigned.
    let service = PtyService::with_config(
        PtyServiceConfig::new(std::env::temp_dir()).with_buffer_limit(1_000),
    );
    let session = spawn_script(
        &service,
        "i=0; while [ $i -lt 400 ]; do printf '中文测试内容'; i=$((i+1)); done; printf '\\n'",
    );
    let id = session.id;

    wait_for_exit(&service, &id);
    let retained = service
        .retained_output(&id)
        .expect("the session is retained");
    assert!(
        retained.total_written > 1_000,
        "the buffer was never overrun"
    );

    let text = std::str::from_utf8(&retained.bytes);
    assert!(
        text.is_ok(),
        "the retained bytes are not valid UTF-8, so a replay would render mojibake: {:?}",
        text.err()
    );
    assert!(
        text.unwrap_or_default().contains('中'),
        "the retained window should still be the Chinese text"
    );
}

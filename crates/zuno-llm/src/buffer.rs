//! How much capacity a reusable framing buffer keeps between frames.
//!
//! # The failure this closes
//!
//! Every incremental framer in this workspace owns one buffer, appends each
//! network chunk to it, and `drain`s a complete frame back off the front.
//! `drain` and `clear` both keep the allocation, so the buffer's capacity is the
//! high-water mark of the largest frame the stream ever carried — and it keeps
//! that capacity for the rest of the stream even though its `len` has returned
//! to zero.
//!
//! Measured on [`crate::sse::SseParser`]: one 4 MiB event left **8,388,608 bytes
//! of capacity with `len() == 0`**, and 1,000 subsequent small events never
//! released a byte of it. That is 0.68% of M1's 1,198,872 KiB tuned-jemalloc
//! W-real median in `docs/perf-methodology.md`, held per live stream, and 1.6x
//! the largest prepared TUI frame R5 measured at 5,156,568 bytes.
//!
//! # Why an allocator setting does not answer it
//!
//! `.cargo/config.toml` gives jemalloc `dirty_decay_ms:1000,muzzy_decay_ms:1000`,
//! so *free* pages return to the OS within about a second without anyone asking.
//! That is why this module shrinks and does not purge: decay already returns
//! pages once they are free, and retained capacity is not free. The shrink is
//! what makes the pages free at all.
//!
//! # The trade
//!
//! Shrinking costs one reallocation the next time a frame exceeds
//! [`STEADY_STATE_CAPACITY_BYTES`]. Keeping a floor rather than shrinking to
//! `len` is what stops a normal stream from paying that on every frame.

/// Capacity a reusable framing buffer keeps once an oversized frame has drained.
///
/// Protects against: one large frame pinning its whole capacity resident for the
/// remaining life of a stream, which on a long-running interactive process is
/// most of the process.
///
/// 64 KiB sits above every steady-state frame this workspace produces, so an
/// ordinary stream never reaches the shrink at all: SSE decodes in 8 KiB chunks
/// ([`crate::sse::SSE_DECODE_CHUNK_BYTES`]), and the largest per-event caps are
/// enforced elsewhere. Holding it costs 65,536 bytes per live buffer, which is
/// 0.0053% of the W-real median above — against the 8,388,608 bytes measured
/// without it, a 128x reduction in what one stream can strand.
pub const STEADY_STATE_CAPACITY_BYTES: usize = 64 * 1024;

/// Release capacity a text framing buffer no longer needs, keeping the floor.
///
/// A no-op unless the buffer grew past [`STEADY_STATE_CAPACITY_BYTES`], so the
/// steady-state path never reallocates.
pub fn release_text_capacity(buffer: &mut String) {
    if buffer.capacity() > STEADY_STATE_CAPACITY_BYTES {
        buffer.shrink_to(STEADY_STATE_CAPACITY_BYTES);
    }
}

/// Release capacity a byte framing buffer no longer needs, keeping the floor.
///
/// A no-op unless the buffer grew past [`STEADY_STATE_CAPACITY_BYTES`], so the
/// steady-state path never reallocates.
pub fn release_byte_capacity(buffer: &mut Vec<u8>) {
    if buffer.capacity() > STEADY_STATE_CAPACITY_BYTES {
        buffer.shrink_to(STEADY_STATE_CAPACITY_BYTES);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn releasing_a_drained_text_buffer_keeps_only_the_steady_state_floor() {
        let mut buffer = "x".repeat(4 * 1024 * 1024);
        buffer.clear();
        assert!(buffer.capacity() > STEADY_STATE_CAPACITY_BYTES);

        release_text_capacity(&mut buffer);

        assert_eq!(buffer.capacity(), STEADY_STATE_CAPACITY_BYTES);
    }

    #[test]
    fn releasing_a_drained_byte_buffer_keeps_only_the_steady_state_floor() {
        let mut buffer = vec![0_u8; 4 * 1024 * 1024];
        buffer.clear();
        assert!(buffer.capacity() > STEADY_STATE_CAPACITY_BYTES);

        release_byte_capacity(&mut buffer);

        assert_eq!(buffer.capacity(), STEADY_STATE_CAPACITY_BYTES);
    }

    #[test]
    fn a_buffer_still_holding_a_partial_frame_keeps_room_for_it() {
        let live = 2 * 1024 * 1024;
        let mut buffer = "y".repeat(live);

        release_text_capacity(&mut buffer);

        assert_eq!(buffer.len(), live);
        assert!(
            buffer.capacity() >= live,
            "the shrink truncated a partial frame's room, capacity {} for {live} live bytes",
            buffer.capacity()
        );
    }

    #[test]
    fn a_steady_state_buffer_is_never_reallocated() {
        let mut buffer = String::with_capacity(STEADY_STATE_CAPACITY_BYTES);
        let before = buffer.as_ptr();
        for _ in 0..1_000 {
            buffer.push_str("data: {\"delta\":\"tok\"}\n\n");
            buffer.clear();
            release_text_capacity(&mut buffer);
        }
        assert_eq!(
            buffer.as_ptr(),
            before,
            "the steady-state path moved its allocation, so every frame paid a realloc"
        );
        assert_eq!(buffer.capacity(), STEADY_STATE_CAPACITY_BYTES);
    }
}

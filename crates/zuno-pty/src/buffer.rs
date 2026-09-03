//! The bounded scrollback one PTY session retains.
//!
//! # Why a fixed-capacity ring and not a growing `Vec`
//!
//! This crate exists because the TypeScript build exhausts memory, and a PTY is
//! the easiest place in an agent to leak it: a single `yes` left running produces
//! output faster than any consumer reads it, forever. The oracle bounds the same
//! buffer at [`BUFFER_LIMIT`] (`packages/core/src/pty.ts:14`) but implements the
//! bound as `buffer = buffer.slice(excess)` (`:220`) — a fresh allocation and a
//! full copy of the retained 2 MiB *per chunk* once the cap is reached. At an
//! 8 KiB read size that is ~250 GiB of memcpy per gigabyte of output.
//!
//! A fixed-capacity ring makes the bound structural instead of periodic: the
//! allocation never exceeds [`ScrollbackBuffer::limit`] bytes, and a write is two
//! `copy_from_slice` calls regardless of how much has been discarded.
//!
//! # Why the head is realigned to a UTF-8 boundary
//!
//! Discarding an arbitrary byte count from the front splits multi-byte code
//! points, and the first thing a client does with `replay` is decode it as text.
//! The oracle counts `BUFFER_LIMIT` in UTF-16 code units and slices a JavaScript
//! string, so it can split a surrogate pair the same way. Since a terminal in this
//! workspace routinely carries Chinese, the common case is mojibake at the top of
//! every replay, so after any discard the head advances past continuation bytes.
//!
//! The scan is bounded at [`MAX_CONTINUATION_BYTES`] because a PTY also carries
//! binary output, where a "continuation byte" is just a byte: an unbounded scan
//! would eat the whole buffer looking for a lead byte that never comes.

/// Retained bytes per session, from `packages/core/src/pty.ts:14`.
///
/// The oracle's constant counts UTF-16 code units of a JavaScript string; this
/// one counts bytes, which is the quantity that actually bounds memory. For ASCII
/// they agree exactly; for CJK text this retains fewer characters and the same
/// number of bytes, which is the correct reading of a memory cap.
pub const BUFFER_LIMIT: usize = 1024 * 1024 * 2;

/// A UTF-8 sequence is at most four bytes, so at most three continuations follow.
const MAX_CONTINUATION_BYTES: usize = 3;

/// First physical size, so a short-lived session costs 8 KiB rather than 2 MiB.
///
/// This matters at the retention cap: 25 retained exited sessions eagerly holding
/// [`BUFFER_LIMIT`] would reserve 50 MiB for output nobody is reading.
const INITIAL_PHYSICAL: usize = 8 * 1024;

/// Where a subscriber wants its replay to begin.
///
/// Mirrors the three cases the oracle's `AttachInput.cursor` encodes
/// (`packages/core/src/pty.ts:224-231`): absent, `-1`, or an absolute cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReplayCursor {
    /// Everything still retained. The oracle's absent `cursor`.
    #[default]
    Full,
    /// Nothing; live output only. The oracle's `cursor: -1`.
    Tail,
    /// From an absolute output cursor, clamped forward to what is still retained.
    From(u64),
}

/// Retained output plus the absolute cursor a client should resume from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replay {
    /// Retained bytes from the requested cursor to the current end.
    pub bytes: Vec<u8>,
    /// Absolute output cursor after these bytes.
    pub cursor: u64,
}

/// A PTY's retained output, bounded by construction.
#[derive(Debug)]
pub struct ScrollbackBuffer {
    storage: Vec<u8>,
    head: usize,
    len: usize,
    limit: usize,
    start_cursor: u64,
    end_cursor: u64,
}

impl Default for ScrollbackBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl ScrollbackBuffer {
    /// Creates a buffer bounded at [`BUFFER_LIMIT`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_limit(BUFFER_LIMIT)
    }

    /// Creates a buffer bounded at `limit` bytes.
    ///
    /// A zero limit is raised to one: the ring's arithmetic needs a non-empty
    /// physical buffer, and "retain nothing" is better expressed by not reading.
    #[must_use]
    pub fn with_limit(limit: usize) -> Self {
        Self {
            storage: Vec::new(),
            head: 0,
            len: 0,
            limit: limit.max(1),
            start_cursor: 0,
            end_cursor: 0,
        }
    }

    /// Restores a retained tail whose absolute output length is already known.
    ///
    /// Background executions persist their complete output to disk but retain only
    /// this ring in memory. After a process restart they read at most `limit` bytes
    /// from the file's tail and use its file length as `total_written`; this
    /// constructor preserves the absolute cursor space without allocating the
    /// discarded prefix.
    #[must_use]
    pub fn restore_tail(limit: usize, total_written: u64, retained_tail: &[u8]) -> Self {
        let mut buffer = Self::with_limit(limit);
        let tail = if retained_tail.len() > buffer.limit {
            &retained_tail[retained_tail.len() - buffer.limit..]
        } else {
            retained_tail
        };
        buffer.push(tail);
        let missing = total_written.saturating_sub(buffer.end_cursor);
        buffer.start_cursor = buffer.start_cursor.saturating_add(missing);
        buffer.end_cursor = buffer.end_cursor.saturating_add(missing);
        buffer
    }

    /// Appends one chunk, discarding whatever oldest bytes no longer fit.
    ///
    /// Never allocates beyond [`Self::limit`], and never blocks or fails, so the
    /// reader thread cannot be stalled by a full buffer.
    pub fn push(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        self.end_cursor = self.end_cursor.saturating_add(chunk.len() as u64);

        // A chunk larger than the whole buffer can only leave its tail behind, and
        // the bytes ahead of that tail are discarded without ever being stored.
        let (chunk, skipped) = if chunk.len() > self.limit {
            (&chunk[chunk.len() - self.limit..], chunk.len() - self.limit)
        } else {
            (chunk, 0)
        };
        self.start_cursor = self.start_cursor.saturating_add(skipped as u64);

        self.grow_for(self.len + chunk.len());
        let overflow = (self.len + chunk.len()).saturating_sub(self.storage.len());
        if overflow > 0 {
            self.discard_front(overflow);
        }
        self.write_tail(chunk);
        if overflow > 0 || skipped > 0 {
            self.align_head();
        }

        debug_assert_eq!(
            self.end_cursor - self.start_cursor,
            self.len as u64,
            "the retained length must always equal the cursor span"
        );
        debug_assert!(
            self.len <= self.limit,
            "the retained length exceeded the limit"
        );
    }

    /// Retained bytes from `cursor` to the current end.
    ///
    /// A cursor older than what is retained is clamped forward to the oldest
    /// retained byte rather than rejected, matching `Math.max(0, from - start)`
    /// at `packages/core/src/pty.ts:236`. The returned [`Replay::cursor`] is
    /// therefore authoritative and a caller must adopt it rather than assume its
    /// request was honoured. What was discarded is not lost when the output was also
    /// persisted; recovering it is the persisted file's job, not this ring's.
    #[must_use]
    pub fn replay(&self, cursor: ReplayCursor) -> Replay {
        self.replay_window(cursor, None)
    }

    /// [`Self::replay`] bounded to at most `limit` bytes.
    ///
    /// For a caller that has to decide how much of a command's output to carry, not
    /// only where to start: a 2 MiB replay is a legitimate answer to "everything since
    /// my cursor" and a ruinous one to hand a model in a single tool result.
    ///
    /// A window that stops short of the end is trimmed back to a UTF-8 boundary, so
    /// paging never splits a code point across two reads for the same reason the head
    /// is realigned after a discard. A window that reaches the end is returned as it
    /// is: there is no following read to align with, and a PTY also carries binary.
    #[must_use]
    pub fn replay_window(&self, cursor: ReplayCursor, limit: Option<usize>) -> Replay {
        let requested = match cursor {
            ReplayCursor::Full => 0,
            ReplayCursor::Tail => self.end_cursor,
            ReplayCursor::From(value) => value,
        };
        let from = requested.clamp(self.start_cursor, self.end_cursor);
        let offset = usize::try_from(from - self.start_cursor).unwrap_or(self.len);
        let mut bytes = self.bytes_from(offset.min(self.len));
        if let Some(limit) = limit
            && bytes.len() > limit
        {
            bytes.truncate(limit);
            trim_incomplete_tail(&mut bytes);
        }
        Replay {
            cursor: from + bytes.len() as u64,
            bytes,
        }
    }

    /// Every retained byte.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        self.bytes_from(0)
    }

    /// Retained bytes. Always at most [`Self::limit`].
    #[must_use]
    pub const fn retained_len(&self) -> usize {
        self.len
    }

    /// Whether nothing is retained.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The configured ceiling on retained bytes.
    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }

    /// Bytes reserved for the ring, which is what a memory assertion should read.
    ///
    /// Distinct from [`Self::retained_len`]: the ring grows geometrically to
    /// [`Self::limit`] and never shrinks, so this is the high-water mark and is
    /// itself bounded by the limit.
    #[must_use]
    pub fn reserved_bytes(&self) -> usize {
        self.storage.capacity()
    }

    /// Absolute cursor of the oldest retained byte. The oracle's `bufferCursor`.
    #[must_use]
    pub const fn start_cursor(&self) -> u64 {
        self.start_cursor
    }

    /// Absolute cursor just past the newest byte. The oracle's `cursor`.
    #[must_use]
    pub const fn end_cursor(&self) -> u64 {
        self.end_cursor
    }

    /// Total bytes ever produced, retained or not.
    #[must_use]
    pub const fn total_written(&self) -> u64 {
        self.end_cursor
    }

    /// Total bytes discarded to stay within the limit.
    #[must_use]
    pub const fn discarded(&self) -> u64 {
        self.start_cursor
    }

    fn grow_for(&mut self, needed: usize) {
        let target = needed.min(self.limit);
        if target <= self.storage.len() {
            return;
        }
        let mut physical = self.storage.len().max(INITIAL_PHYSICAL.min(self.limit));
        while physical < target {
            physical = physical.saturating_mul(2).min(self.limit);
        }

        let mut grown = vec![0u8; physical];
        let (front, back) = self.slices();
        grown[..front.len()].copy_from_slice(front);
        grown[front.len()..front.len() + back.len()].copy_from_slice(back);
        self.storage = grown;
        self.head = 0;
    }

    fn discard_front(&mut self, count: usize) {
        let count = count.min(self.len);
        if count == 0 {
            return;
        }
        self.head = (self.head + count) % self.storage.len();
        self.len -= count;
        self.start_cursor = self.start_cursor.saturating_add(count as u64);
    }

    fn write_tail(&mut self, chunk: &[u8]) {
        debug_assert!(
            self.len + chunk.len() <= self.storage.len(),
            "write_tail requires the chunk to already fit"
        );
        let physical = self.storage.len();
        let tail = (self.head + self.len) % physical;
        let contiguous = physical - tail;
        if chunk.len() <= contiguous {
            self.storage[tail..tail + chunk.len()].copy_from_slice(chunk);
        } else {
            self.storage[tail..].copy_from_slice(&chunk[..contiguous]);
            self.storage[..chunk.len() - contiguous].copy_from_slice(&chunk[contiguous..]);
        }
        self.len += chunk.len();
    }

    fn align_head(&mut self) {
        for _ in 0..MAX_CONTINUATION_BYTES {
            match self.byte_at(0) {
                Some(byte) if is_continuation(byte) => self.discard_front(1),
                _ => return,
            }
        }
    }

    fn byte_at(&self, offset: usize) -> Option<u8> {
        if offset >= self.len {
            return None;
        }
        let physical = self.storage.len();
        self.storage.get((self.head + offset) % physical).copied()
    }

    fn slices(&self) -> (&[u8], &[u8]) {
        if self.len == 0 {
            return (&[], &[]);
        }
        let physical = self.storage.len();
        let end = self.head + self.len;
        if end <= physical {
            (&self.storage[self.head..end], &[])
        } else {
            (&self.storage[self.head..], &self.storage[..end - physical])
        }
    }

    fn bytes_from(&self, offset: usize) -> Vec<u8> {
        let (front, back) = self.slices();
        let mut out = Vec::with_capacity(self.len.saturating_sub(offset));
        if offset < front.len() {
            out.extend_from_slice(&front[offset..]);
            out.extend_from_slice(back);
        } else {
            let into_back = offset - front.len();
            if into_back < back.len() {
                out.extend_from_slice(&back[into_back..]);
            }
        }
        out
    }
}

const fn is_continuation(byte: u8) -> bool {
    byte & 0b1100_0000 == 0b1000_0000
}

/// Drops an incomplete UTF-8 sequence from the end of one window.
///
/// The tail counterpart of [`ScrollbackBuffer::align_head`], and the same rationale:
/// the first thing a client does with a window is decode it. Only a tail that is the
/// prefix of a longer sequence is dropped, so binary output keeps every byte it
/// produced, and the window is never emptied — a limit smaller than one code point
/// still has to make progress or a caller paging by the returned cursor would ask for
/// the same window forever.
pub(crate) fn trim_incomplete_tail(bytes: &mut Vec<u8>) {
    if let Err(error) = std::str::from_utf8(bytes)
        && error.error_len().is_none()
        && error.valid_up_to() > 0
    {
        bytes.truncate(error.valid_up_to());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn an_unfilled_buffer_retains_everything_in_order() {
        let mut buffer = ScrollbackBuffer::with_limit(64);
        buffer.push(b"hello ");
        buffer.push(b"world");
        assert_eq!(buffer.to_bytes(), b"hello world");
        assert_eq!(buffer.retained_len(), 11);
        assert_eq!(buffer.discarded(), 0);
        assert_eq!(buffer.total_written(), 11);
    }

    #[test]
    fn overflow_discards_the_oldest_bytes_and_keeps_the_tail() {
        let mut buffer = ScrollbackBuffer::with_limit(8);
        buffer.push(b"0123456789ab");
        assert_eq!(buffer.to_bytes(), b"456789ab");
        assert_eq!(buffer.retained_len(), 8);
        assert_eq!(buffer.discarded(), 4);
        assert_eq!(buffer.total_written(), 12);
    }

    #[test]
    fn the_retained_length_never_exceeds_the_limit_across_many_wraps() {
        let mut buffer = ScrollbackBuffer::with_limit(16);
        for index in 0u32..2_000 {
            buffer.push(&index.to_le_bytes());
            assert!(
                buffer.retained_len() <= 16,
                "retained {}",
                buffer.retained_len()
            );
            assert!(
                buffer.reserved_bytes() <= 16,
                "reserved {}",
                buffer.reserved_bytes()
            );
        }
        assert_eq!(buffer.total_written(), 8_000);
        assert_eq!(buffer.retained_len(), 16);
        let tail: Vec<u8> = (1_996u32..2_000)
            .flat_map(|index| index.to_le_bytes())
            .collect();
        assert_eq!(buffer.to_bytes(), tail);
    }

    #[test]
    fn a_single_chunk_larger_than_the_limit_keeps_only_its_tail() {
        let mut buffer = ScrollbackBuffer::with_limit(4);
        buffer.push(b"abcdefghij");
        assert_eq!(buffer.to_bytes(), b"ghij");
        assert_eq!(buffer.discarded(), 6);
        assert_eq!(buffer.total_written(), 10);
    }

    #[test]
    fn the_head_is_realigned_so_a_replay_never_starts_mid_code_point() {
        // Four three-byte code points; a 10-byte limit cuts one apart.
        let mut buffer = ScrollbackBuffer::with_limit(10);
        buffer.push("中文测试".as_bytes());
        assert_eq!(
            buffer.retained_len(),
            9,
            "one whole code point must have been dropped"
        );
        assert_eq!(
            std::str::from_utf8(&buffer.to_bytes()),
            Ok("文测试"),
            "the retained bytes must decode without a replacement character"
        );
        assert_eq!(buffer.discarded(), 3);
    }

    #[test]
    fn realignment_is_bounded_on_binary_output() {
        // Every byte looks like a UTF-8 continuation, so an unbounded scan would
        // empty the buffer instead of dropping at most three bytes.
        let mut buffer = ScrollbackBuffer::with_limit(8);
        buffer.push(&[0x80u8; 32]);
        assert_eq!(
            buffer.retained_len(),
            5,
            "at most three bytes may be given up"
        );
    }

    #[test]
    fn replay_variants_resolve_against_the_retained_window() {
        let mut buffer = ScrollbackBuffer::with_limit(8);
        buffer.push(b"0123456789");

        let full = buffer.replay(ReplayCursor::Full);
        assert_eq!(full.bytes, b"23456789");
        assert_eq!(full.cursor, 10);

        let tail = buffer.replay(ReplayCursor::Tail);
        assert!(tail.bytes.is_empty());
        assert_eq!(tail.cursor, 10);

        assert_eq!(buffer.replay(ReplayCursor::From(5)).bytes, b"56789");
        assert_eq!(
            buffer.replay(ReplayCursor::From(0)).bytes,
            b"23456789",
            "a cursor older than the window clamps forward"
        );
        assert!(
            buffer.replay(ReplayCursor::From(99)).bytes.is_empty(),
            "a cursor past the end yields nothing rather than panicking"
        );
    }

    #[test]
    fn a_bounded_window_reports_the_cursor_the_next_one_starts_at() {
        let mut buffer = ScrollbackBuffer::with_limit(32);
        buffer.push(b"0123456789");

        let first = buffer.replay_window(ReplayCursor::Full, Some(4));
        assert_eq!(first.bytes, b"0123");
        assert_eq!(first.cursor, 4, "an unbounded replay ended at 10");

        let second = buffer.replay_window(ReplayCursor::From(first.cursor), Some(4));
        assert_eq!(second.bytes, b"4567");
        assert_eq!(second.cursor, 8);

        let last = buffer.replay_window(ReplayCursor::From(second.cursor), Some(4));
        assert_eq!(last.bytes, b"89");
        assert_eq!(last.cursor, 10);
        assert_eq!(
            buffer
                .replay_window(ReplayCursor::From(last.cursor), Some(4))
                .bytes,
            b"",
            "a cursor at the end returns nothing rather than repeating the tail"
        );
    }

    #[test]
    fn a_bounded_window_short_of_the_end_stops_on_a_code_point() {
        // Three-byte code points read four bytes at a time: an untrimmed window would
        // end mid-character and both sides of the cut would decode as U+FFFD.
        let mut buffer = ScrollbackBuffer::with_limit(32);
        buffer.push("中文测试".as_bytes());

        let mut cursor = 0u64;
        let mut decoded = String::new();
        while cursor < buffer.end_cursor() {
            let window = buffer.replay_window(ReplayCursor::From(cursor), Some(4));
            decoded.push_str(std::str::from_utf8(&window.bytes).expect("window decodes alone"));
            assert!(!window.bytes.is_empty(), "the window has to advance");
            cursor = window.cursor;
        }

        assert_eq!(decoded, "中文测试");
    }

    #[test]
    fn a_window_that_reaches_the_end_keeps_bytes_that_are_not_text() {
        let mut buffer = ScrollbackBuffer::with_limit(32);
        buffer.push(&[b'a', 0xe4]);

        let window = buffer.replay_window(ReplayCursor::Full, Some(8));

        assert_eq!(
            window.bytes,
            [b'a', 0xe4],
            "there is no next window to align with, so nothing may be dropped"
        );
        assert_eq!(window.cursor, 2);
    }

    #[test]
    fn a_zero_limit_is_raised_rather_than_dividing_by_zero() {
        let mut buffer = ScrollbackBuffer::with_limit(0);
        buffer.push(b"abc");
        assert_eq!(buffer.limit(), 1);
        assert_eq!(buffer.to_bytes(), b"c");
    }

    #[test]
    fn a_restored_tail_keeps_absolute_cursors_without_loading_the_prefix() {
        let buffer = ScrollbackBuffer::restore_tail(8, 100, b"23456789");

        assert_eq!(buffer.to_bytes(), b"23456789");
        assert_eq!(buffer.start_cursor(), 92);
        assert_eq!(buffer.end_cursor(), 100);
        assert_eq!(buffer.discarded(), 92);
        assert_eq!(buffer.replay(ReplayCursor::From(96)).bytes, b"6789");
    }

    #[test]
    fn one_hundred_megabytes_of_output_stays_within_the_two_mebibyte_ceiling() {
        const CHUNK: usize = 8 * 1024;
        const TOTAL: u64 = 100 * 1024 * 1024;

        let before = rss_kib();
        let mut buffer = ScrollbackBuffer::new();
        let chunk = vec![b'x'; CHUNK];
        let mut written = 0u64;
        while written < TOTAL {
            buffer.push(&chunk);
            written += CHUNK as u64;
            assert!(
                buffer.retained_len() <= BUFFER_LIMIT,
                "retained {} exceeded the {BUFFER_LIMIT}-byte limit after {written} bytes",
                buffer.retained_len()
            );
            assert!(
                buffer.reserved_bytes() <= BUFFER_LIMIT,
                "reserved {} exceeded the {BUFFER_LIMIT}-byte limit after {written} bytes",
                buffer.reserved_bytes()
            );
        }

        assert_eq!(buffer.total_written(), written);
        assert!(
            written >= TOTAL,
            "the test must actually push 100 MiB, pushed {written}"
        );
        assert_eq!(buffer.retained_len(), BUFFER_LIMIT);
        assert_eq!(buffer.discarded(), written - BUFFER_LIMIT as u64);

        // Corroboration only. The structural proof is the two assertions inside
        // the loop, which hold after every one of the 12,800 chunks; RSS is
        // page-granular and cannot see a transient excursion.
        if let (Some(before), Some(after)) = (before, rss_kib()) {
            let delta = after.saturating_sub(before);
            assert!(
                delta <= 8 * 1024,
                "RSS grew {delta} KiB while streaming {written} bytes through a \
                 {BUFFER_LIMIT}-byte buffer"
            );
            println!("100 MiB through a 2 MiB scrollback: RSS delta {delta} KiB");
        }
    }
}

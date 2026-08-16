//! The cap on how many exited PTY sessions stay observable.
//!
//! An exited session is deliberately not deleted: a client that reconnects after
//! its terminal died still wants the status, the exit code, and the output that
//! killed it. The oracle says so at `packages/core/src/pty.ts:15-16` — *"Exited
//! sessions stay observable (status, exit code, retained output) until removed
//! explicitly. Cap retention so abandoned terminals do not accumulate unbounded
//! buffers."*
//!
//! Nothing removes them explicitly in practice, though. A user who opens twenty
//! terminals over an afternoon and closes the browser leaves twenty abandoned
//! [`crate::buffer::BUFFER_LIMIT`]-sized buffers behind, which is why the cap
//! exists and why it is enforced here rather than left to a caller.
//!
//! # The eviction order is exit order, not creation order
//!
//! `exitOrder` is appended to when a child exits (`pty.ts:228`) and the oldest
//! *entry* is evicted (`:234-238`), so a session created first but exited last is
//! the last to be evicted. Ordering by creation instead would still keep exactly
//! 25 sessions and would keep the wrong 25 — which is why
//! [`ExitRetention::record_exit`] is a distinct operation from creation and why
//! its test asserts *which* ids survive.

use std::collections::VecDeque;

use crate::session::PtyId;

/// Retained exited sessions, from `packages/core/src/pty.ts:17`.
pub const EXITED_LIMIT: usize = 25;

/// The exit-ordered queue of sessions still retained after their child exited.
#[derive(Debug)]
pub struct ExitRetention {
    order: VecDeque<PtyId>,
    limit: usize,
}

impl Default for ExitRetention {
    fn default() -> Self {
        Self::new()
    }
}

impl ExitRetention {
    /// Creates a queue capped at [`EXITED_LIMIT`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_limit(EXITED_LIMIT)
    }

    /// Creates a queue capped at `limit` exited sessions.
    ///
    /// A zero limit is raised to one. Zero would make a session evict *itself* the
    /// moment it exits, so its exit code could never be read and
    /// [`Self::record_exit`] would be asked to evict the id it was just given —
    /// see the re-entrancy note on that method.
    #[must_use]
    pub fn with_limit(limit: usize) -> Self {
        Self {
            order: VecDeque::new(),
            limit: limit.max(1),
        }
    }

    /// Records that `id`'s child exited and returns the ids that must now be
    /// dropped, oldest exit first.
    ///
    /// Returning the evictions rather than performing them keeps this type free of
    /// any knowledge of processes or locks, and it is what makes the ordering
    /// testable without spawning anything.
    ///
    /// The just-exited id is never in the returned list, because a non-zero limit
    /// guarantees at least one slot for it. The caller relies on that: eviction
    /// runs on the exiting session's own waiter thread, so evicting itself would
    /// mean tearing down the thread doing the tearing down.
    ///
    /// Recording the same id twice is a no-op returning nothing. The oracle
    /// guarantees single delivery with a status check before the push
    /// (`pty.ts:224`); this repeats the guarantee at the queue so a double
    /// notification cannot silently shorten the retained history.
    pub fn record_exit(&mut self, id: PtyId) -> Vec<PtyId> {
        if self.order.contains(&id) {
            return Vec::new();
        }
        self.order.push_back(id);

        let mut evicted = Vec::new();
        while self.order.len() > self.limit {
            match self.order.pop_front() {
                Some(oldest) => evicted.push(oldest),
                None => break,
            }
        }
        evicted
    }

    /// Drops `id` from the queue because it was removed explicitly.
    ///
    /// Mirrors the `indexOf`/`splice` pair in `removeSession` (`pty.ts:143-144`).
    /// Returns whether it was retained, so a caller can tell an exited session
    /// from a running one without a second lookup.
    pub fn forget(&mut self, id: &PtyId) -> bool {
        let before = self.order.len();
        self.order.retain(|retained| retained != id);
        self.order.len() != before
    }

    /// Every retained id, oldest exit first.
    pub fn retained(&self) -> impl Iterator<Item = &PtyId> {
        self.order.iter()
    }

    /// Whether `id` is retained after exiting.
    #[must_use]
    pub fn contains(&self, id: &PtyId) -> bool {
        self.order.contains(id)
    }

    /// How many exited sessions are retained. Never above [`Self::limit`].
    #[must_use]
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Whether no exited session is retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// The configured cap.
    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }

    /// Forgets everything, for service teardown (`pty.ts:132`).
    pub fn clear(&mut self) {
        self.order.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(index: usize) -> PtyId {
        PtyId::from_raw(format!("pty_{index:04}"))
    }

    #[test]
    fn nothing_is_evicted_below_the_limit() {
        let mut retention = ExitRetention::with_limit(3);
        for index in 0..3 {
            assert!(retention.record_exit(id(index)).is_empty());
        }
        assert_eq!(retention.len(), 3);
    }

    #[test]
    fn the_oldest_exit_is_evicted_first_not_the_oldest_creation() {
        let mut retention = ExitRetention::with_limit(3);
        // Exit order deliberately reverses creation order.
        for index in (0..3).rev() {
            assert!(retention.record_exit(id(index)).is_empty());
        }
        assert_eq!(
            retention.record_exit(id(9)),
            vec![id(2)],
            "id 2 exited first"
        );
        let retained: Vec<_> = retention.retained().cloned().collect();
        assert_eq!(retained, vec![id(1), id(0), id(9)]);
    }

    #[test]
    fn thirty_exits_at_the_default_limit_retain_the_last_twenty_five() {
        let mut retention = ExitRetention::new();
        let mut evicted = Vec::new();
        for index in 0..30 {
            evicted.extend(retention.record_exit(id(index)));
        }
        assert_eq!(retention.len(), EXITED_LIMIT);
        assert_eq!(evicted, (0..5).map(id).collect::<Vec<_>>());
        let retained: Vec<_> = retention.retained().cloned().collect();
        assert_eq!(retained, (5..30).map(id).collect::<Vec<_>>());
    }

    #[test]
    fn a_repeated_exit_notification_evicts_nothing() {
        let mut retention = ExitRetention::with_limit(2);
        assert!(retention.record_exit(id(0)).is_empty());
        assert!(retention.record_exit(id(0)).is_empty());
        assert_eq!(
            retention.len(),
            1,
            "the duplicate must not occupy a second slot"
        );
    }

    #[test]
    fn forgetting_an_id_frees_its_slot() {
        let mut retention = ExitRetention::with_limit(2);
        retention.record_exit(id(0));
        retention.record_exit(id(1));
        assert!(retention.forget(&id(0)));
        assert!(
            !retention.forget(&id(0)),
            "forgetting twice reports no removal"
        );
        assert!(retention.record_exit(id(2)).is_empty());
        assert_eq!(
            retention.retained().cloned().collect::<Vec<_>>(),
            vec![id(1), id(2)]
        );
    }

    #[test]
    fn a_zero_limit_is_raised_so_an_exit_never_evicts_itself() {
        let mut retention = ExitRetention::with_limit(0);
        assert_eq!(retention.limit(), 1);
        assert!(retention.record_exit(id(0)).is_empty());
        assert_eq!(retention.record_exit(id(1)), vec![id(0)]);
    }
}

//! Single-use tickets authorizing one PTY WebSocket upgrade.
//!
//! `POST /pty/:ptyID/connect-token` mints one and `GET /pty/:ptyID/connect`
//! redeems it, from `packages/core/src/pty/ticket.ts`. A WebSocket cannot carry a
//! custom header from a browser, so the credential has to travel in the URL — and
//! a URL ends up in history, proxy logs and referrers. A ticket is therefore
//! short-lived, single-use, and bound to the exact session and workspace it was
//! minted for, so a leaked one is worth almost nothing.
//!
//! # Why this lives in the library and not the route
//!
//! The store is the only stateful half, and there are two upgrade surfaces in the
//! oracle (`server/src/handlers/pty.ts` and the experimental HttpApi handler) that
//! must not each keep their own. Todos 64-70 own the routes; they should hold one
//! [`TicketStore`] from the service and call [`TicketStore::issue`] and
//! [`TicketStore::consume`].

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::session::PtyId;

/// Ticket lifetime, from `packages/core/src/pty/ticket.ts:9`.
pub const DEFAULT_TICKET_TTL: Duration = Duration::from_secs(60);

/// Outstanding tickets retained, from `packages/core/src/pty/ticket.ts:10`.
///
/// The cap is what keeps an unauthenticated mint endpoint from being a memory
/// exhaustion vector: 10,000 entries of an identifier and two short strings is
/// well under a megabyte, and the oldest are discarded first.
pub const TICKET_CAPACITY: usize = 10_000;

/// The `connect-token` response, from `packages/schema/src/pty-ticket.ts:6-9`.
///
/// `expires_in` keeps the oracle's snake_case name: it is a wire contract with
/// existing clients, not a Rust naming decision.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConnectToken {
    /// The opaque single-use credential.
    pub ticket: String,
    /// Seconds until the ticket expires.
    pub expires_in: u64,
}

/// What a ticket is valid for, from `packages/core/src/pty/ticket.ts:14-18`.
///
/// All three must match on redemption. Scoping to the workspace as well as the
/// session is what stops a ticket minted against one project's PTY from being
/// replayed against another project that happens to reuse the identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketScope {
    /// The session the ticket authorizes.
    pub pty_id: PtyId,
    /// The request's resolved directory, when the surface routes by directory.
    pub directory: Option<String>,
    /// The request's workspace, when the surface routes by workspace.
    pub workspace_id: Option<String>,
}

impl TicketScope {
    /// A scope covering just a session, for surfaces with no workspace routing.
    #[must_use]
    pub fn for_session(pty_id: PtyId) -> Self {
        Self {
            pty_id,
            directory: None,
            workspace_id: None,
        }
    }
}

#[derive(Debug)]
struct Entry {
    ticket: String,
    scope: TicketScope,
    issued: Instant,
}

/// The outstanding tickets, bounded in both count and lifetime.
#[derive(Debug)]
pub struct TicketStore {
    entries: Mutex<VecDeque<Entry>>,
    ttl: Duration,
    capacity: usize,
}

impl Default for TicketStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TicketStore {
    /// Creates a store with the oracle's TTL and capacity.
    #[must_use]
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_TICKET_TTL, TICKET_CAPACITY)
    }

    /// Creates a store with an explicit TTL and capacity, for tests.
    #[must_use]
    pub fn with_limits(ttl: Duration, capacity: usize) -> Self {
        Self {
            entries: Mutex::new(VecDeque::new()),
            ttl,
            capacity: capacity.max(1),
        }
    }

    /// Mints a ticket for `scope`.
    #[must_use]
    pub fn issue(&self, scope: TicketScope) -> ConnectToken {
        self.issue_at(scope, Instant::now())
    }

    /// Redeems `ticket` for `scope`, returning whether it was valid.
    ///
    /// A valid ticket is consumed, so a replay of the same URL fails.
    pub fn consume(&self, ticket: &str, scope: &TicketScope) -> bool {
        self.consume_at(ticket, scope, Instant::now())
    }

    /// [`Self::issue`] against an explicit clock reading.
    ///
    /// Time is a parameter so the expiry rules are tested as pure functions rather
    /// than by sleeping for a TTL.
    #[must_use]
    pub fn issue_at(&self, scope: TicketScope, now: Instant) -> ConnectToken {
        let ticket = uuid::Uuid::new_v4().to_string();
        let mut entries = self.lock();
        prune_expired(&mut entries, self.ttl, now);
        while entries.len() >= self.capacity {
            entries.pop_front();
        }
        entries.push_back(Entry {
            ticket: ticket.clone(),
            scope,
            issued: now,
        });
        ConnectToken {
            ticket,
            expires_in: self.ttl.as_secs().max(1),
        }
    }

    /// [`Self::consume`] against an explicit clock reading.
    pub fn consume_at(&self, ticket: &str, scope: &TicketScope, now: Instant) -> bool {
        let mut entries = self.lock();
        prune_expired(&mut entries, self.ttl, now);
        let Some(position) = entries
            .iter()
            .position(|entry| entry.ticket == ticket && &entry.scope == scope)
        else {
            return false;
        };
        entries.remove(position).is_some()
    }

    /// Discards every ticket minted for `pty_id`, for when the session goes away.
    ///
    /// Not in the oracle, which lets an orphaned ticket sit until its TTY expires.
    /// Dropping them with the session means a removed PTY's tickets cannot be
    /// redeemed against a later session that reuses the identifier.
    pub fn revoke_session(&self, pty_id: &PtyId) {
        self.lock().retain(|entry| &entry.scope.pty_id != pty_id);
    }

    /// Outstanding tickets, for asserting the cap holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no ticket is outstanding.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// The configured cap on outstanding tickets.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn prune_expired(entries: &mut VecDeque<Entry>, ttl: Duration, now: Instant) {
    while entries
        .front()
        .is_some_and(|entry| now.saturating_duration_since(entry.issued) >= ttl)
    {
        entries.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> TicketScope {
        TicketScope::for_session(PtyId::from_raw("pty_one"))
    }

    #[test]
    fn a_ticket_is_valid_once() {
        let store = TicketStore::new();
        let token = store.issue(scope());
        assert_eq!(token.expires_in, 60);
        assert!(store.consume(&token.ticket, &scope()));
        assert!(
            !store.consume(&token.ticket, &scope()),
            "a replayed ticket must be rejected"
        );
        assert!(store.is_empty());
    }

    #[test]
    fn a_ticket_is_rejected_for_a_different_session_or_workspace() {
        let store = TicketStore::new();
        let token = store.issue(scope());
        let other_session = TicketScope::for_session(PtyId::from_raw("pty_two"));
        assert!(!store.consume(&token.ticket, &other_session));

        let other_workspace = TicketScope {
            workspace_id: Some("wrk_other".to_owned()),
            ..scope()
        };
        assert!(!store.consume(&token.ticket, &other_workspace));
        assert!(
            store.consume(&token.ticket, &scope()),
            "the rejections must not have consumed it"
        );
    }

    #[test]
    fn an_expired_ticket_is_rejected_and_pruned() {
        let store = TicketStore::with_limits(Duration::from_secs(60), 8);
        let issued = Instant::now();
        let token = store.issue_at(scope(), issued);
        let expired = issued + Duration::from_secs(60);
        assert!(!store.consume_at(&token.ticket, &scope(), expired));
        assert!(
            store.is_empty(),
            "expiry must reclaim the slot, not leak it"
        );
    }

    #[test]
    fn a_ticket_just_inside_the_ttl_is_still_valid() {
        let store = TicketStore::new();
        let issued = Instant::now();
        let token = store.issue_at(scope(), issued);
        assert!(store.consume_at(&token.ticket, &scope(), issued + Duration::from_secs(59)));
    }

    #[test]
    fn the_outstanding_count_never_exceeds_the_capacity() {
        let store = TicketStore::with_limits(Duration::from_secs(600), 4);
        let mut tokens = Vec::new();
        for _ in 0..64 {
            tokens.push(store.issue(scope()));
            assert!(store.len() <= 4, "outstanding {}", store.len());
        }
        assert_eq!(store.len(), 4);
        assert!(
            !store.consume(&tokens[0].ticket, &scope()),
            "an evicted ticket must not be redeemable"
        );
        let newest = tokens.last().expect("tokens were issued");
        assert!(store.consume(&newest.ticket, &scope()));
    }

    #[test]
    fn revoking_a_session_drops_only_its_tickets() {
        let store = TicketStore::new();
        let mine = store.issue(scope());
        let other_scope = TicketScope::for_session(PtyId::from_raw("pty_two"));
        let theirs = store.issue(other_scope.clone());
        store.revoke_session(&PtyId::from_raw("pty_one"));
        assert!(!store.consume(&mine.ticket, &scope()));
        assert!(store.consume(&theirs.ticket, &other_scope));
    }

    #[test]
    fn a_zero_capacity_is_raised_so_a_ticket_survives_its_own_issue() {
        let store = TicketStore::with_limits(DEFAULT_TICKET_TTL, 0);
        assert_eq!(store.capacity(), 1);
        let token = store.issue(scope());
        assert!(store.consume(&token.ticket, &scope()));
    }
}

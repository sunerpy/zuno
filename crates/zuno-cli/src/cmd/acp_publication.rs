//! Per-connection delivery fences for native turn projections.
//!
//! These guards track pending publication, never execution success. A prompt
//! still needs its durable receipt outcome; this fence only prevents that outcome
//! from overtaking the corresponding text, tool checkpoints and work snapshots.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;
use zuno_engine::r#loop::TurnEvent;

#[derive(Default)]
struct PendingPublication {
    input_ids: BTreeSet<String>,
    turn_ids: BTreeSet<String>,
}

impl PendingPublication {
    fn includes(&self, input_id: &str, turn_id: Option<&str>) -> bool {
        self.input_ids.contains(input_id)
            || turn_id.is_some_and(|turn_id| self.turn_ids.contains(turn_id))
            // Until TurnStarted is consumed, even a known owning input cannot
            // rule out a receipt steered into this drive. Once identified, an
            // unrelated turn no longer blocks this observer.
            || self.turn_ids.is_empty()
    }
}

#[derive(Default)]
struct PublicationState {
    next_id: u64,
    pending: BTreeMap<u64, PendingPublication>,
}

#[derive(Default)]
pub(super) struct TurnPublications {
    state: Mutex<PublicationState>,
    changed: Notify,
}

impl TurnPublications {
    pub(super) fn begin(self: &Arc<Self>, input_id: Option<&str>) -> PublicationPass {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.next_id = state
            .next_id
            .checked_add(1)
            .expect("publication ID space exhausted");
        let id = state.next_id;
        let mut pending = PendingPublication::default();
        if let Some(input_id) = input_id {
            pending.input_ids.insert(input_id.to_owned());
        }
        state.pending.insert(id, pending);
        PublicationPass {
            publications: Arc::clone(self),
            id,
        }
    }

    pub(super) async fn wait_for(&self, input_id: &str, turn_id: Option<&str>) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let pending = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pending
                .values()
                .any(|pending| pending.includes(input_id, turn_id));
            if !pending {
                return;
            }
            changed.await;
        }
    }
}

pub(super) struct PublicationPass {
    publications: Arc<TurnPublications>,
    id: u64,
}

impl PublicationPass {
    pub(super) fn observe(&self, event: &TurnEvent) {
        let mut state = self
            .publications
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(pending) = state.pending.get_mut(&self.id) else {
            return;
        };
        match event {
            TurnEvent::TurnStarted { turn_id, .. } => {
                pending.turn_ids.insert(turn_id.clone());
            }
            TurnEvent::InputConsumed { input_id, .. } => {
                pending.input_ids.insert(input_id.clone());
            }
            _ => {}
        }
        drop(state);
        self.publications.changed.notify_waiters();
    }
}

impl Drop for PublicationPass {
    fn drop(&mut self) {
        self.publications
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .remove(&self.id);
        self.publications.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn steered_receipt_waits_until_the_projection_identifies_its_native_turn() {
        let publications = Arc::new(TurnPublications::default());
        let pass = publications.begin(Some("input-owner"));
        let waiter = publications.wait_for("input-steered", Some("turn"));
        tokio::pin!(waiter);
        assert!(
            futures::poll!(&mut waiter).is_pending(),
            "a terminal steered receipt cannot overtake the first queued TurnStarted"
        );
        pass.observe(&TurnEvent::TurnStarted {
            session_id: "session".to_owned(),
            turn_id: "turn".to_owned(),
        });
        assert!(futures::poll!(&mut waiter).is_pending());
        drop(pass);
        assert!(futures::poll!(&mut waiter).is_ready());
    }

    #[tokio::test]
    async fn publication_waits_for_its_turn_without_waiting_for_a_later_turn() {
        let publications = Arc::new(TurnPublications::default());
        let first = publications.begin(Some("input-first"));
        first.observe(&TurnEvent::TurnStarted {
            session_id: "session".to_owned(),
            turn_id: "turn-first".to_owned(),
        });
        let later = publications.begin(Some("input-later"));
        later.observe(&TurnEvent::TurnStarted {
            session_id: "session".to_owned(),
            turn_id: "turn-later".to_owned(),
        });
        let waiter = publications.wait_for("input-steered", Some("turn-first"));
        tokio::pin!(waiter);
        assert!(futures::poll!(&mut waiter).is_pending());
        drop(first);
        assert!(
            futures::poll!(&mut waiter).is_ready(),
            "the completed projection must not wait for unrelated later work"
        );
        drop(later);
    }

    #[tokio::test]
    async fn already_drained_publication_does_not_lose_its_wakeup() {
        let publications = Arc::new(TurnPublications::default());
        let pass = publications.begin(Some("input"));
        let waiter = publications.wait_for("input", Some("turn"));
        drop(pass);
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("a completed send before first poll must remain observable");
    }
}

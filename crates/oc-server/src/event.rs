//! One bounded queue per event-stream connection.
//!
//! The overflow policy is **drop newest**. Existing queued transitions stay in
//! order; each new transition that cannot fit increments one scalar counter. Once
//! the backlog drains, the subscriber receives [`Delivery::Lagged`] before any
//! later event. If publishing stops while the subscriber is stalled, `recv` still
//! returns the lag count after the retained backlog, so loss is never silent.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use oc_engine::r#loop::TurnEvent;
use tokio::sync::{Notify, mpsc};

/// Queue slots per connection.
///
/// This matches the engine's 64-transition channel: one connection can absorb a
/// complete producer backlog, but a client slower than that must resynchronize
/// rather than retaining an arbitrarily stale second copy of the turn.
pub const DEFAULT_EVENT_SUBSCRIBER_CAPACITY: usize = 64;

/// One item observed by an event-stream connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delivery<E> {
    /// A retained event. `Arc` shares one payload across every connection.
    Event(Arc<E>),
    /// New events were dropped while this connection's queue was full.
    Lagged {
        /// Events dropped since the previous lag notification.
        dropped: u64,
    },
}

/// Bounded event distributor shared by all route handlers.
pub struct EventFanout<E> {
    inner: Arc<FanoutInner<E>>,
}

impl<E> Clone for EventFanout<E> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<E> std::fmt::Debug for EventFanout<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EventFanout")
            .field("capacity", &self.inner.capacity)
            .field("subscribers", &self.subscriber_count())
            .finish()
    }
}

impl<E> EventFanout<E> {
    /// Creates a distributor. Zero is raised to one so publishing remains useful.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Arc::new(FanoutInner {
                capacity: capacity.max(1),
                next_token: AtomicU64::new(1),
                subscribers: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Registers one connection with an independent fixed queue.
    #[must_use]
    pub fn subscribe(&self) -> EventSubscription<E> {
        let token = self.inner.next_token.fetch_add(1, Ordering::Relaxed);
        let queue = Arc::new(SubscriberQueue::new(self.inner.capacity));
        self.inner
            .lock_subscribers()
            .insert(token, Arc::clone(&queue));
        EventSubscription {
            token,
            queue,
            owner: Arc::downgrade(&self.inner),
        }
    }

    /// Publishes without waiting for any connection.
    ///
    /// The payload is allocated once and every subscriber queues an `Arc` clone.
    /// A stalled connection therefore cannot block the turn or duplicate a large
    /// tool event per subscriber.
    pub fn publish(&self, event: E) {
        let event = Arc::new(event);
        for queue in self.inner.lock_subscribers().values() {
            queue.push(Arc::clone(&event));
        }
    }

    /// Current connection count, for shutdown and diagnostics.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.inner.lock_subscribers().len()
    }
}

impl EventFanout<TurnEvent> {
    /// Drains the engine's lossless channel into bounded connection queues.
    ///
    /// `run_turn` keeps lossless backpressure up to this seam. Beyond it, one slow
    /// network peer degrades only its own queue and learns exactly how many
    /// transitions it lost.
    pub async fn forward_engine_events(&self, mut events: mpsc::Receiver<TurnEvent>) {
        while let Some(event) = events.recv().await {
            self.publish(event);
        }
    }
}

impl<E> Default for EventFanout<E> {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_EVENT_SUBSCRIBER_CAPACITY)
    }
}

/// One event-stream connection.
///
/// Dropping this value immediately removes its queue from the fan-out map, so a
/// disconnected peer cannot retain future events.
pub struct EventSubscription<E> {
    token: u64,
    queue: Arc<SubscriberQueue<E>>,
    owner: Weak<FanoutInner<E>>,
}

impl<E> std::fmt::Debug for EventSubscription<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EventSubscription")
            .field("token", &self.token)
            .field("queued", &self.queued())
            .field("capacity", &self.capacity())
            .finish()
    }
}

impl<E> EventSubscription<E> {
    /// Receives the next retained event or explicit loss marker.
    pub async fn recv(&mut self) -> Option<Delivery<E>> {
        loop {
            // Create the waiter before checking state. A publish between these two
            // operations stores the notification in this future instead of being
            // lost between an empty check and `.await`.
            let notified = self.queue.notify.notified();
            if let Some(delivery) = self.queue.pop() {
                return Some(delivery);
            }
            if self.queue.is_closed() {
                return None;
            }
            notified.await;
        }
    }

    /// Number of concrete deliveries held in memory right now.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.queue.lock().queued.len()
    }

    /// Hard queue-depth ceiling for this connection.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.queue.capacity
    }
}

impl<E> Drop for EventSubscription<E> {
    fn drop(&mut self) {
        self.queue.close();
        if let Some(owner) = self.owner.upgrade() {
            owner.lock_subscribers().remove(&self.token);
        }
    }
}

struct FanoutInner<E> {
    capacity: usize,
    next_token: AtomicU64,
    subscribers: Mutex<HashMap<u64, Arc<SubscriberQueue<E>>>>,
}

impl<E> FanoutInner<E> {
    fn lock_subscribers(&self) -> MutexGuard<'_, HashMap<u64, Arc<SubscriberQueue<E>>>> {
        self.subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl<E> Drop for FanoutInner<E> {
    fn drop(&mut self) {
        let subscribers = self
            .subscribers
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for queue in subscribers.values() {
            queue.close();
        }
    }
}

struct SubscriberQueue<E> {
    capacity: usize,
    state: Mutex<QueueState<E>>,
    notify: Notify,
}

struct QueueState<E> {
    queued: VecDeque<Delivery<E>>,
    pending_dropped: u64,
    closed: bool,
}

impl<E> SubscriberQueue<E> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(QueueState {
                queued: VecDeque::with_capacity(capacity),
                pending_dropped: 0,
                closed: false,
            }),
            notify: Notify::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, QueueState<E>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn push(&self, event: Arc<E>) {
        let mut state = self.lock();
        if state.closed {
            return;
        }

        if state.pending_dropped > 0 {
            if state.queued.len() < self.capacity {
                let dropped = std::mem::take(&mut state.pending_dropped);
                state.queued.push_back(Delivery::Lagged { dropped });
            } else {
                state.pending_dropped = state.pending_dropped.saturating_add(1);
                return;
            }
        }

        if state.queued.len() < self.capacity {
            state.queued.push_back(Delivery::Event(event));
            drop(state);
            self.notify.notify_one();
        } else {
            state.pending_dropped = state.pending_dropped.saturating_add(1);
        }
    }

    fn pop(&self) -> Option<Delivery<E>> {
        let mut state = self.lock();
        if let Some(delivery) = state.queued.pop_front() {
            return Some(delivery);
        }
        if state.pending_dropped > 0 {
            return Some(Delivery::Lagged {
                dropped: std::mem::take(&mut state.pending_dropped),
            });
        }
        None
    }

    fn close(&self) {
        self.lock().closed = true;
        self.notify.notify_waiters();
    }

    fn is_closed(&self) -> bool {
        self.lock().closed
    }
}

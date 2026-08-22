use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::sync::Notify;

/// A soft interruption to inject at the next safe point in the turn loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoftInterruptMessage {
    /// Durable inbox id, when this message was admitted before wake-up.
    pub input_id: Option<String>,
    pub content: String,
    pub images: Vec<(String, String)>,
    /// Whether the turn loop may skip remaining tools before injecting this message.
    pub urgent: bool,
    pub source: SoftInterruptSource,
}

/// The producer of a soft interruption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoftInterruptSource {
    User,
    System,
    BackgroundTask,
}

/// A reset-safe interruption signal readable by sync code and awaitable by async code.
#[derive(Debug, Clone)]
pub struct InterruptSignal {
    flag: Arc<AtomicBool>,
    epoch: Arc<AtomicU64>,
    notify: Arc<Notify>,
}

impl InterruptSignal {
    /// Creates a signal in the clear state at epoch zero.
    #[must_use]
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            epoch: Arc::new(AtomicU64::new(0)),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Fires the signal and wakes every currently registered asynchronous waiter.
    pub fn fire(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Reads the fired state without requiring a Tokio runtime.
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Returns the monotonic fire epoch to pair with [`Self::reset_if_epoch`].
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// Clears the signal only when no newer [`Self::fire`] has occurred.
    ///
    /// A fire racing between the first epoch check and the flag clear is restored and
    /// broadcast again. The return value is `true` only when this reset was applied.
    #[must_use]
    pub fn reset_if_epoch(&self, epoch: u64) -> bool {
        if self.epoch.load(Ordering::SeqCst) != epoch {
            return false;
        }

        self.flag.store(false, Ordering::SeqCst);

        if self.epoch.load(Ordering::SeqCst) != epoch {
            self.flag.store(true, Ordering::SeqCst);
            self.notify.notify_waiters();
            return false;
        }

        true
    }

    /// Sleeps until this signal is fired, or returns immediately when already fired.
    pub async fn notified(&self) {
        let mut notified = std::pin::pin!(self.notify.notified());

        // notify_waiters does not retain a permit. Register before the flag re-check so
        // fire cannot land between that check and the future's first poll and be lost.
        notified.as_mut().enable();
        if self.is_set() {
            return;
        }

        notified.await;
    }

    /// Reports whether two handles share the same underlying signal state.
    #[must_use]
    pub fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.flag, &other.flag)
    }
}

impl Default for InterruptSignal {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl zuno_tool::InterruptHandle for InterruptSignal {
    fn is_set(&self) -> bool {
        InterruptSignal::is_set(self)
    }

    async fn notified(&self) {
        InterruptSignal::notified(self).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    #[tokio::test]
    async fn interrupt_notify_waiters_tracks_creation_and_stores_no_permit() {
        let notify = tokio::sync::Notify::new();

        let created_before = notify.notified();
        notify.notify_waiters();
        tokio::time::timeout(Duration::from_millis(100), created_before)
            .await
            .expect("a Notified future created before notify_waiters must receive the wakeup");

        let mut enabled_before = std::pin::pin!(notify.notified());
        enabled_before.as_mut().enable();
        notify.notify_waiters();
        tokio::time::timeout(Duration::from_millis(100), enabled_before)
            .await
            .expect("an enabled Notified future must receive notify_waiters");

        notify.notify_waiters();
        let created_after = notify.notified();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), created_after)
                .await
                .is_err(),
            "notify_waiters must not store a permit for a future waiter"
        );
    }

    #[test]
    fn interrupt_fire_never_loses_wakeup_while_notified_races() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_time()
            .build()
            .expect("multi-threaded runtime");

        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(20), async {
                for iteration in 0..2_000 {
                    let signal = InterruptSignal::new();
                    let start = Arc::new(Barrier::new(2));
                    let waiter = {
                        let signal = signal.clone();
                        let start = Arc::clone(&start);
                        tokio::spawn(async move {
                            start.wait();
                            signal.notified().await;
                        })
                    };

                    start.wait();
                    signal.fire();

                    tokio::time::timeout(Duration::from_millis(250), waiter)
                        .await
                        .unwrap_or_else(|_| {
                            panic!("lost wakeup on iteration {iteration}: notified missed fire")
                        })
                        .expect("waiter task must not panic");
                }
            })
            .await
            .expect("lost-wakeup hammer exceeded its internal timeout");
        });
    }

    #[test]
    fn interrupt_reset_if_epoch_never_erases_concurrent_fire() {
        for iteration in 0..2_000 {
            let signal = InterruptSignal::new();
            signal.fire();
            let epoch = signal.epoch();

            let firer = {
                let signal = signal.clone();
                std::thread::spawn(move || signal.fire())
            };
            let _reset_applied = signal.reset_if_epoch(epoch);
            firer.join().expect("firer thread must not panic");

            assert!(
                signal.is_set(),
                "concurrent fire was erased on iteration {iteration}"
            );
        }
    }

    #[test]
    fn interrupt_is_sync_readable_without_a_runtime() {
        let signal = InterruptSignal::new();
        assert!(!signal.is_set());
        signal.fire();
        assert!(signal.is_set());
    }

    #[test]
    fn interrupt_reset_if_epoch_rejects_a_stale_reset() {
        let signal = InterruptSignal::new();
        signal.fire();
        let first_epoch = signal.epoch();
        signal.fire();

        assert!(!signal.reset_if_epoch(first_epoch));
        assert!(signal.is_set());

        let current_epoch = signal.epoch();
        assert!(signal.reset_if_epoch(current_epoch));
        assert!(!signal.is_set());
        assert!(!signal.reset_if_epoch(first_epoch));
    }

    #[test]
    fn interrupt_same_instance_only_matches_clones() {
        let signal = InterruptSignal::new();
        let cloned = signal.clone();
        let independent = InterruptSignal::new();

        assert!(signal.same_instance(&cloned));
        assert!(!signal.same_instance(&independent));
    }
}

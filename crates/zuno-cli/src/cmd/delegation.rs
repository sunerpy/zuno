//! Shared concurrency admission for native and product-agent delegations.

use std::collections::VecDeque;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// One process-local delegation capacity shared by every turn host in a workspace.
#[derive(Debug, Clone)]
pub(crate) struct DelegationLimiter {
    inner: Arc<DelegationLimiterInner>,
}

#[derive(Debug)]
struct DelegationLimiterInner {
    state: Mutex<DelegationLimiterState>,
}

#[derive(Debug)]
struct DelegationLimiterState {
    limit: usize,
    active: usize,
    waiters: VecDeque<oneshot::Sender<DelegationPermit>>,
}

impl DelegationLimiter {
    pub(crate) fn new(limit: NonZeroUsize) -> Self {
        Self {
            inner: Arc::new(DelegationLimiterInner {
                state: Mutex::new(DelegationLimiterState {
                    limit: limit.get(),
                    active: 0,
                    waiters: VecDeque::new(),
                }),
            }),
        }
    }

    /// Apply the latest workspace configuration without detaching existing work.
    ///
    /// Lowering the limit never cancels active delegations. New callers wait until
    /// the active count falls below the new bound; raising it wakes queued callers.
    pub(crate) fn set_limit(&self, limit: NonZeroUsize) {
        let changed = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.limit == limit.get() {
                false
            } else {
                state.limit = limit.get();
                true
            }
        };
        if changed {
            Self::dispatch_waiters(&self.inner);
        }
    }

    pub(crate) async fn acquire(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<DelegationPermit, DelegationAcquireError> {
        if cancellation.is_cancelled() {
            return Err(DelegationAcquireError::Cancelled);
        }
        let mut immediate = None;
        let mut queued = None;
        {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while state
                .waiters
                .front()
                .is_some_and(oneshot::Sender::is_closed)
            {
                state.waiters.pop_front();
            }
            if state.active < state.limit && state.waiters.is_empty() {
                state.active = state.active.saturating_add(1);
                immediate = Some(DelegationPermit::new(Arc::clone(&self.inner)));
            } else {
                let (sender, receiver) = oneshot::channel();
                state.waiters.push_back(sender);
                queued = Some(receiver);
            }
        }
        if let Some(permit) = immediate {
            if cancellation.is_cancelled() {
                drop(permit);
                return Err(DelegationAcquireError::Cancelled);
            }
            return Ok(permit);
        }
        Self::dispatch_waiters(&self.inner);
        let receiver = queued.expect("a non-immediate acquisition is queued");
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(DelegationAcquireError::Cancelled),
            permit = receiver => permit.map_err(|_| DelegationAcquireError::Cancelled),
        }
    }

    fn dispatch_waiters(inner: &Arc<DelegationLimiterInner>) {
        loop {
            let sender = {
                let mut state = inner
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                while state
                    .waiters
                    .front()
                    .is_some_and(oneshot::Sender::is_closed)
                {
                    state.waiters.pop_front();
                }
                if state.active >= state.limit {
                    return;
                }
                let Some(sender) = state.waiters.pop_front() else {
                    return;
                };
                state.active = state.active.saturating_add(1);
                sender
            };
            let permit = DelegationPermit::new(Arc::clone(inner));
            if let Err(mut permit) = sender.send(permit) {
                permit.release(false);
            }
        }
    }

    #[cfg(test)]
    fn waiting_count(&self) -> usize {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .waiters
            .iter()
            .filter(|waiter| !waiter.is_closed())
            .count()
    }
}

/// One active slot. Dropping it is the only way to release delegation capacity.
#[derive(Debug)]
pub(crate) struct DelegationPermit {
    inner: Arc<DelegationLimiterInner>,
    released: bool,
}

impl DelegationPermit {
    fn new(inner: Arc<DelegationLimiterInner>) -> Self {
        Self {
            inner,
            released: false,
        }
    }

    fn release(&mut self, dispatch: bool) {
        if self.released {
            return;
        }
        self.released = true;
        {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.active = state.active.saturating_sub(1);
        }
        if dispatch {
            DelegationLimiter::dispatch_waiters(&self.inner);
        }
    }
}

impl Drop for DelegationPermit {
    fn drop(&mut self) {
        self.release(true);
    }
}

/// Why a delegation could not enter the shared execution budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DelegationAcquireError {
    Cancelled,
}

impl fmt::Display for DelegationAcquireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("delegation was cancelled while waiting"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn clones_share_one_bound_until_the_running_permit_is_released() {
        let limiter = DelegationLimiter::new(NonZeroUsize::new(1).expect("non-zero"));
        let cancellation = CancellationToken::new();
        let running = limiter
            .acquire(&cancellation)
            .await
            .expect("first delegation is admitted");

        let waiting_limiter = limiter.clone();
        let waiting_cancellation = CancellationToken::new();
        let waiting =
            tokio::spawn(async move { waiting_limiter.acquire(&waiting_cancellation).await });
        tokio::task::yield_now().await;
        assert!(
            !waiting.is_finished(),
            "a second delegation exceeded the bound"
        );

        drop(running);
        let _second = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("released capacity wakes the waiter")
            .expect("waiter task survives")
            .expect("second delegation is admitted");
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_wait_without_consuming_capacity() {
        let limiter = DelegationLimiter::new(NonZeroUsize::new(1).expect("non-zero"));
        let running_cancellation = CancellationToken::new();
        let running = limiter
            .acquire(&running_cancellation)
            .await
            .expect("first delegation is admitted");

        let waiting_limiter = limiter.clone();
        let waiting_cancellation = CancellationToken::new();
        let cancellation = waiting_cancellation.clone();
        let waiting =
            tokio::spawn(async move { waiting_limiter.acquire(&waiting_cancellation).await });
        tokio::task::yield_now().await;
        cancellation.cancel();

        let outcome = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("cancellation wakes the waiter")
            .expect("waiter task survives");
        assert!(matches!(outcome, Err(DelegationAcquireError::Cancelled)));
        drop(running);

        let _next = limiter
            .acquire(&CancellationToken::new())
            .await
            .expect("a cancelled waiter did not leak the permit");
    }

    #[tokio::test]
    async fn lowering_the_bound_waits_for_active_work_without_detaching_it() {
        let limiter = DelegationLimiter::new(NonZeroUsize::new(2).expect("non-zero"));
        let first = limiter
            .acquire(&CancellationToken::new())
            .await
            .expect("first delegation");
        let second = limiter
            .acquire(&CancellationToken::new())
            .await
            .expect("second delegation");
        limiter.set_limit(NonZeroUsize::new(1).expect("non-zero"));

        let waiting_limiter = limiter.clone();
        let waiting =
            tokio::spawn(async move { waiting_limiter.acquire(&CancellationToken::new()).await });
        drop(first);
        tokio::task::yield_now().await;
        assert!(
            !waiting.is_finished(),
            "one remaining active delegation still fills the lowered bound"
        );

        drop(second);
        let _third = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("the final release wakes the waiter")
            .expect("waiter task survives")
            .expect("waiting delegation is admitted");
    }

    #[tokio::test]
    async fn queued_delegations_are_admitted_fifo_without_barging() {
        let limiter = DelegationLimiter::new(NonZeroUsize::new(1).expect("non-zero"));
        let running = limiter
            .acquire(&CancellationToken::new())
            .await
            .expect("first delegation is admitted");

        let waiting_limiter = limiter.clone();
        let (admitted_tx, admitted_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let waiting = tokio::spawn(async move {
            let permit = waiting_limiter
                .acquire(&CancellationToken::new())
                .await
                .expect("queued delegation is admitted");
            let _ = admitted_tx.send(());
            let _ = release_rx.await;
            drop(permit);
        });
        while limiter.waiting_count() != 1 {
            tokio::task::yield_now().await;
        }

        drop(running);
        let barger = tokio::time::timeout(
            Duration::from_millis(30),
            limiter.acquire(&CancellationToken::new()),
        )
        .await;
        assert!(
            barger.is_err(),
            "a newly polled caller barged ahead of the older queued delegation"
        );
        tokio::time::timeout(Duration::from_secs(1), admitted_rx)
            .await
            .expect("the oldest waiter receives released capacity")
            .expect("the waiter remains alive");
        let _ = release_tx.send(());
        waiting.await.expect("waiter task survives");

        let _next = limiter
            .acquire(&CancellationToken::new())
            .await
            .expect("a timed-out barger does not leak capacity");
    }
}

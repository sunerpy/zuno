//! Shared concurrency admission for native and product-agent delegations.

use std::fmt;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// One process-local delegation capacity shared by every turn host in a workspace.
#[derive(Debug, Clone)]
pub(crate) struct DelegationLimiter {
    inner: Arc<DelegationLimiterInner>,
}

#[derive(Debug)]
struct DelegationLimiterInner {
    state: Mutex<DelegationLimiterState>,
    changed: watch::Sender<u64>,
}

#[derive(Debug)]
struct DelegationLimiterState {
    limit: usize,
    active: usize,
}

impl DelegationLimiter {
    pub(crate) fn new(limit: NonZeroUsize) -> Self {
        let (changed, _receiver) = watch::channel(0);
        Self {
            inner: Arc::new(DelegationLimiterInner {
                state: Mutex::new(DelegationLimiterState {
                    limit: limit.get(),
                    active: 0,
                }),
                changed,
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
            self.notify_changed();
        }
    }

    pub(crate) async fn acquire(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<DelegationPermit, DelegationAcquireError> {
        let mut changed = self.inner.changed.subscribe();
        loop {
            {
                let mut state = self
                    .inner
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.active < state.limit {
                    state.active = state.active.saturating_add(1);
                    return Ok(DelegationPermit {
                        inner: Arc::clone(&self.inner),
                    });
                }
            }
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    return Err(DelegationAcquireError::Cancelled);
                }
                _ = changed.changed() => {}
            }
        }
    }

    fn notify_changed(&self) {
        self.inner.changed.send_modify(|generation| {
            *generation = generation.wrapping_add(1);
        });
    }
}

/// One active slot. Dropping it is the only way to release delegation capacity.
#[derive(Debug)]
pub(crate) struct DelegationPermit {
    inner: Arc<DelegationLimiterInner>,
}

impl Drop for DelegationPermit {
    fn drop(&mut self) {
        {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.active = state.active.saturating_sub(1);
        }
        self.inner.changed.send_modify(|generation| {
            *generation = generation.wrapping_add(1);
        });
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
}

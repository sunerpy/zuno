//! The cancellation signal a running `rg` process polls.
//!
//! Deliberately a local trait rather than a dependency on `zuno-tool`'s
//! `InterruptHandle`: a search is useful outside a tool call (the LSP file walk in
//! todo 48 wants the same engine), and this crate should not need the tool layer to
//! be linked in to run. `zuno-tools` supplies a one-method adapter.
//!
//! The method is synchronous because process supervision runs at a platform
//! boundary with no dependency on the tool layer or a Tokio runtime.

/// A cancellation signal process supervision can poll.
pub trait Cancellation: Send + Sync {
    /// Whether cancellation has been requested.
    fn is_cancelled(&self) -> bool;
}

/// A signal that never fires. For direct use and for tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct NeverCancelled;

impl Cancellation for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
}

/// A signal that is already fired. For tests that must prove the poll happens.
#[derive(Debug, Clone, Copy, Default)]
pub struct AlreadyCancelled;

impl Cancellation for AlreadyCancelled {
    fn is_cancelled(&self) -> bool {
        true
    }
}

impl<T: Cancellation + ?Sized> Cancellation for &T {
    fn is_cancelled(&self) -> bool {
        (**self).is_cancelled()
    }
}

impl<T: Cancellation + ?Sized> Cancellation for std::sync::Arc<T> {
    fn is_cancelled(&self) -> bool {
        (**self).is_cancelled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Shaped like `zuno_engine::InterruptSignal`: a sync read over shared state.
    #[derive(Default)]
    struct Firable(Arc<AtomicBool>);

    impl Cancellation for Firable {
        fn is_cancelled(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }

    #[test]
    fn a_shared_flag_satisfies_the_trait_with_no_runtime() {
        let flag = Arc::new(AtomicBool::new(false));
        let signal = Firable(Arc::clone(&flag));

        assert!(!signal.is_cancelled());
        flag.store(true, Ordering::SeqCst);
        assert!(signal.is_cancelled());
    }

    #[test]
    fn an_arc_forwards_to_its_contents() {
        let signal: Arc<dyn Cancellation> = Arc::new(AlreadyCancelled);
        assert!(signal.is_cancelled());

        let never: Arc<dyn Cancellation> = Arc::new(NeverCancelled);
        assert!(!never.is_cancelled());
    }
}

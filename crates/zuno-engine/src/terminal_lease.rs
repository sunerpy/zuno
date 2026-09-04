//! Exclusive, time-bounded ownership of the one shared terminal.
//!
//! # The deadlock this exists to prevent
//!
//! An external editor, authentication flow, or subprocess may need cooked stdin
//! while the Rust TUI owns the same TTY in raw mode inside the alternate screen.
//! Run both at once and the subprocess waits forever for a line it will never
//! receive while the render loop consumes the user's typing. This is a runtime
//! deadlock, not a rendering glitch.
//!
//! The fix is a lease. Exactly one party may hold the terminal; a host asks for it,
//! the owner suspends and yields the TTY, the host prompts, and the lease is
//! returned. Every way a host can touch the terminal is expressed as "hold a lease",
//! so the failure mode becomes a refusal or a reclaim carrying a diagnostic instead
//! of a hang.
//!
//! # Why the protocol lives here and not in `zuno-tui`
//!
//! Both sides speak it: a requester acquires and `zuno-tui` grants. Putting the
//! protocol in `zuno-tui` would force every terminal client to depend on ratatui
//! merely to ask for stdin. `zuno-engine` is below both the clients and the TUI,
//! so it owns the state machine while higher crates provide concrete terminal
//! transitions.
//!
//! A consequence worth stating: **nothing here touches a terminal.** No `crossterm`,
//! no ioctl, no `isatty`. This module is the state machine and the vocabulary; the
//! two physical transitions belong to [`TerminalOwner`]. This separation lets
//! clients prove their behavior against `zuno_testkit::FakeTerminalOwner` without
//! requiring a real TTY.
//!
//! # Shape
//!
//! | piece | role |
//! |---|---|
//! | [`TerminalLease`] | what a host calls: `acquire(reason) -> Result<Guard>` |
//! | [`TerminalLeaseGuard`] | the held lease; releases on `Drop` |
//! | [`TerminalOwner`] | the two physical transitions the terminal's owner performs |
//! | [`TerminalBroker`] | the protocol itself: exclusion, deadline, force-reclaim |
//!
//! [`TerminalBroker`] implements [`TerminalLease`] over any [`TerminalOwner`], so the
//! mutual exclusion, the deadline and the diagnostic are written once. An owner free
//! to implement [`TerminalLease`] itself would also be free to get the policy subtly
//! wrong, which is the duplication this split refuses.
//!
//! # Child cleanup invariant
//!
//! A lease may register [`TerminalLeaseCleanup`] when its holder gives the inherited
//! TTY to a child process. Normal release and deadline reclaim run that cleanup first.
//! If cleanup cannot confirm that the child was terminated and reaped, the broker
//! fails closed: **the TTY is never reclaimed while a child that inherited it is alive.**

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::oneshot;

/// How long a host may hold the terminal before it is taken back.
///
/// Sized for the thing that actually holds it: a human reading a device code off a
/// browser and typing it back. Five minutes is generous for that and still short
/// enough that a wedged requester does not strand the session. Override with
/// [`TerminalBroker::with_timeout`]; tests must, so that no test waits a production
/// interval.
pub const DEFAULT_LEASE_TIMEOUT: Duration = Duration::from_secs(300);

/// Why a host wants the terminal, and on whose behalf.
///
/// The requester name is carried separately from the human-readable purpose because the
/// force-reclaim diagnostic has to name a culprit. "A lease expired" is not
/// actionable; "requester `kiro` did not release it" is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseReason {
    /// The requester the lease is granted to. Appears in every diagnostic.
    pub requester: String,
    /// What it is about to do, phrased for a user who sees the TUI step aside.
    pub purpose: String,
}

impl LeaseReason {
    /// A reason naming the requester and its purpose.
    #[must_use]
    pub fn new(requester: impl Into<String>, purpose: impl Into<String>) -> Self {
        Self {
            requester: requester.into(),
            purpose: purpose.into(),
        }
    }
}

impl fmt::Display for LeaseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "requester `{}` ({})", self.requester, self.purpose)
    }
}

/// A refused lease.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TerminalLeaseError {
    /// Someone else holds the terminal and their deadline has not passed.
    ///
    /// Refusal, not queueing — see [`TerminalBroker`] for the argument.
    #[error(
        "the terminal is held by requester `{holder}` ({holder_purpose}); \
         requester `{requested_by}` cannot prompt until it is released"
    )]
    Busy {
        /// The requester currently holding the lease.
        holder: String,
        /// What the holder said it was doing.
        holder_purpose: String,
        /// The requester that was refused.
        requested_by: String,
    },

    /// The owner could not give the terminal up.
    ///
    /// Distinct from [`Self::Busy`]: nobody holds the lease, the terminal itself is
    /// unavailable — no TTY, a render loop that will not stop, a mode restore that
    /// failed. The host must not prompt.
    #[error("requester `{requested_by}` was not given the terminal: {detail}")]
    Unavailable {
        /// The requester that asked.
        requested_by: String,
        /// The owner's explanation, rendered into the message.
        detail: String,
    },
}

/// Why a lease ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReclaimCause {
    /// The guard was dropped. The normal path.
    Released,
    /// The deadline passed with the guard still alive.
    ///
    /// Carries the diagnostic rather than leaving each owner to compose one, so every
    /// surface reports the same sentence about the same failure.
    Deadline(ForcedReclaim),
}

impl ReclaimCause {
    /// Whether this end was involuntary.
    #[must_use]
    pub fn is_forced(&self) -> bool {
        matches!(self, Self::Deadline(_))
    }
}

/// The diagnostic for a lease taken back by force.
///
/// A struct rather than a preformatted string, so a caller can log fields and a human
/// can read [`fmt::Display`] without the two drifting apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForcedReclaim {
    /// The requester that failed to release. The point of the whole type.
    pub requester: String,
    /// What the requester said it was doing when it took the lease.
    pub purpose: String,
    /// The deadline it blew.
    pub timeout: Duration,
}

impl fmt::Display for ForcedReclaim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "requester `{}` held the terminal for `{}` past its {} ms deadline \
             and did not release it; the terminal was reclaimed by force",
            self.requester,
            self.purpose,
            self.timeout.as_millis()
        )
    }
}

/// The two physical transitions performed by whoever owns the terminal.
///
/// Implemented by the TUI in todo 73 (leave the alternate screen, restore cooked
/// mode, stop reading keys — then the reverse), and by
/// `zuno_testkit::FakeTerminalOwner` for everything that has to be proven without a
/// terminal.
///
/// # Why `reclaim_terminal` is synchronous
///
/// It runs on [`TerminalLeaseGuard`]'s `Drop`, and `Drop` cannot await. A guard whose
/// release needed to await would have to block a runtime thread or spawn a detached
/// task whose completion nobody could observe — and a release that may not have
/// happened yet is exactly the deadlock this module removes. The restore path is a
/// handful of terminal writes, so it is honestly synchronous. This mirrors
/// [`zuno_tool::InterruptHandle::is_set`], synchronous for the same class of reason:
/// the caller has no runtime to lend it.
///
/// `yield_terminal` may await, because acquisition happens in async host code and a
/// real owner has to let its render loop reach a safe point first.
#[async_trait]
pub trait TerminalOwner: Send + Sync + 'static {
    /// Gives the terminal up. Called once per granted lease, before the guard exists.
    ///
    /// Returning `Err` refuses the lease: the slot is left vacant and the host gets
    /// [`TerminalLeaseError::Unavailable`] carrying this string. A refusal must leave
    /// the terminal exactly as it was.
    async fn yield_terminal(&self, reason: &LeaseReason) -> Result<(), String>;

    /// Takes the terminal back. Called exactly once per granted lease.
    ///
    /// `cause` distinguishes an orderly return from a force-reclaim. On
    /// [`ReclaimCause::Deadline`] the owner receives the [`ForcedReclaim`] diagnostic
    /// and is responsible for surfacing it: this crate has no logging facade, and
    /// inventing one here would put a presentation decision in the wrong layer.
    fn reclaim_terminal(&self, reason: &LeaseReason, cause: ReclaimCause);
}

/// The terminal, as a host sees it.
///
/// A host holds `&dyn TerminalLease` and never learns whether a TUI, a plain stdio
/// session, or a test fake is on the other side.
#[async_trait]
pub trait TerminalLease: Send + Sync {
    /// Takes exclusive ownership of the terminal, or explains why not.
    ///
    /// The returned guard *is* the lease: hold it for the prompt, drop it when done.
    /// See [`TerminalBroker`] for what happens if you do not.
    async fn acquire(&self, reason: LeaseReason) -> Result<TerminalLeaseGuard, TerminalLeaseError>;

    /// Takes the terminal with a mandatory pre-reclaim cleanup hook.
    ///
    /// A holder that starts a child inheriting the TTY must use this form. The hook may
    /// return `Ok` only after that child has exited and been reaped.
    async fn acquire_with_cleanup(
        &self,
        reason: LeaseReason,
        cleanup: Arc<dyn TerminalLeaseCleanup>,
    ) -> Result<TerminalLeaseGuard, TerminalLeaseError>;
}

/// Cleanup that must succeed before an inherited TTY can return to its owner.
pub trait TerminalLeaseCleanup: Send + Sync + 'static {
    /// Terminates and reaps the holder's child, or refuses terminal reclaim.
    fn before_reclaim(&self) -> Result<(), String>;
}

/// The lease that is currently out, if any.
struct Held {
    reason: LeaseReason,
    /// When the deadline passes. Compared against [`Instant::now`], so a loaded
    /// machine can only be late, never early.
    deadline: Instant,
    timeout: Duration,
    /// Set by whichever of release-or-reclaim gets there first. The loser then does
    /// nothing, which is what makes `reclaim_terminal` exactly-once under a race.
    settled: Arc<AtomicBool>,
    cleanup: Option<Arc<dyn TerminalLeaseCleanup>>,
}

struct ReclaimWork {
    reason: LeaseReason,
    diagnostic: ForcedReclaim,
    settled: Arc<AtomicBool>,
    cleanup: Option<Arc<dyn TerminalLeaseCleanup>>,
}

/// State shared by the broker, every outstanding guard, and the watchdog task.
///
/// Separate from [`TerminalBroker`] so a guard can clear the slot on `Drop` and a
/// watchdog can outlive the call that spawned it, without the broker itself having
/// to be reachable only through an `Arc`.
struct SharedSlot {
    owner: Arc<dyn TerminalOwner>,
    slot: Mutex<Option<Held>>,
}

impl SharedSlot {
    /// The slot lock, treating poisoning as recoverable.
    ///
    /// Poisoning here means an unrelated panic unwound while the slot was locked. The
    /// slot is a plain `Option`, so it cannot be observed half-updated; refusing to
    /// serve the terminal for the rest of the process would be a worse outcome than
    /// continuing from a consistent value.
    fn locked(&self) -> MutexGuard<'_, Option<Held>> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Ends one lease, returning whether this caller is the one that must reclaim.
    ///
    /// The slot entry is cleared by whoever arrives — winner or loser — because a
    /// settled lease is dead either way, and leaving it in place would let the next
    /// `acquire` refuse against a holder that no longer exists. The identity check
    /// keeps a late arrival from evicting the *next* holder.
    fn settle(&self, settled: &Arc<AtomicBool>) -> bool {
        let won = !settled.swap(true, Ordering::SeqCst);
        if !won {
            return false;
        }
        let mut slot = self.locked();
        if slot
            .as_ref()
            .is_some_and(|held| Arc::ptr_eq(&held.settled, settled))
        {
            slot.take();
        }
        true
    }

    /// Marks an expired holder as reclaiming while leaving its slot occupied.
    ///
    /// Keeping the slot prevents a second acquirer from yielding the same TTY while
    /// registered child cleanup is still proving that the old holder is gone.
    fn begin_expired_reclaim(&self, now: Instant) -> Option<ReclaimWork> {
        let slot = self.locked();
        let held = match slot.as_ref() {
            Some(held) if now >= held.deadline && !held.settled.swap(true, Ordering::SeqCst) => {
                held
            }
            _ => return None,
        };
        let diagnostic = ForcedReclaim {
            requester: held.reason.requester.clone(),
            purpose: held.reason.purpose.clone(),
            timeout: held.timeout,
        };
        Some(ReclaimWork {
            reason: held.reason.clone(),
            diagnostic,
            settled: Arc::clone(&held.settled),
            cleanup: held.cleanup.clone(),
        })
    }

    fn finish_expired_reclaim(&self, settled: &Arc<AtomicBool>) -> bool {
        let mut slot = self.locked();
        if slot
            .as_ref()
            .is_some_and(|held| Arc::ptr_eq(&held.settled, settled))
        {
            slot.take();
            true
        } else {
            false
        }
    }

    fn abandon_expired_reclaim(&self, settled: &Arc<AtomicBool>) {
        let slot = self.locked();
        if slot
            .as_ref()
            .is_some_and(|held| Arc::ptr_eq(&held.settled, settled))
        {
            settled.store(false, Ordering::SeqCst);
        }
    }

    /// Reclaims an expired lease. Returns the diagnostic raised, if one was.
    fn reclaim_if_expired(&self) -> Option<ForcedReclaim> {
        let work = self.begin_expired_reclaim(Instant::now())?;
        if let Some(cleanup) = &work.cleanup
            && cleanup.before_reclaim().is_err()
        {
            self.abandon_expired_reclaim(&work.settled);
            return None;
        }
        if !self.finish_expired_reclaim(&work.settled) {
            return None;
        }
        self.owner.reclaim_terminal(
            &work.reason,
            ReclaimCause::Deadline(work.diagnostic.clone()),
        );
        Some(work.diagnostic)
    }
}

/// The protocol: one holder at a time, with a deadline.
///
/// # The concurrent-acquire policy is refusal
///
/// A second `acquire` while a live lease is out fails immediately with
/// [`TerminalLeaseError::Busy`], naming the holder. Neither queueing nor preemption
/// was chosen, for reasons specific to what a lease is *for*:
///
/// 1. **A queued prompt is a wrong prompt.** What is being serialized is a human
///    typing into a device-code field. A second prompt that appears whenever the
///    first happens to finish arrives with no context, on a terminal the user has
///    since moved on from, and is indistinguishable from the first. Refusing lets the
///    host say "another requester is prompting" *now*.
/// 2. **Queueing hides the deadlock this module removes.** A host that never releases
///    would make every later acquirer block, so the symptom would again be a hang —
///    one level further out — and the force-reclaim would only free the head of the
///    queue. A refusal is observable at the call site, which is where a requester author
///    can act on it.
/// 3. **Preemption corrupts the thing being protected.** Revoking a lease mid-prompt
///    yanks the terminal out from under half-typed input. Preemption is reserved for
///    the one case where the holder has already broken the contract: the deadline.
///
/// So the only involuntary transition is the deadline, and it is loud.
///
/// # Two paths reclaim an expired lease, and that is deliberate
///
/// A watchdog task fires at the deadline, so a wedged requester is reclaimed even when
/// nothing else ever happens. And `acquire` sweeps an expired holder before deciding
/// busy-or-grant, so a leaked guard cannot wedge the terminal permanently even if the
/// watchdog never ran — no Tokio runtime at acquire time, a runtime shut down while
/// the lease was out, a task never polled. Both funnel through one settle-once flag,
/// so a lease is reclaimed exactly once whichever path arrives first.
pub struct TerminalBroker {
    shared: Arc<SharedSlot>,
    timeout: Duration,
}

impl TerminalBroker {
    /// A broker over `owner` with the [`DEFAULT_LEASE_TIMEOUT`].
    #[must_use]
    pub fn new(owner: Arc<dyn TerminalOwner>) -> Self {
        Self::with_timeout(owner, DEFAULT_LEASE_TIMEOUT)
    }

    /// A broker with an explicit deadline.
    ///
    /// The injection point tests use: a lease that must be force-reclaimed gets a
    /// deadline of milliseconds, one that must not gets a deadline no test run could
    /// reach. Both directions are then immune to load, because a busy machine can
    /// only make a timer late.
    #[must_use]
    pub fn with_timeout(owner: Arc<dyn TerminalOwner>, timeout: Duration) -> Self {
        Self {
            shared: Arc::new(SharedSlot {
                owner,
                slot: Mutex::new(None),
            }),
            timeout,
        }
    }

    /// The deadline this broker grants leases with.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Whether a lease is currently out, expired or not.
    ///
    /// For assertions, and for a caller that would rather avoid a refusal it can
    /// predict. Never a precondition for `acquire`: another task can win between the
    /// check and the call, which is why `acquire` decides under the lock instead of
    /// trusting this.
    #[must_use]
    pub fn is_held(&self) -> bool {
        self.shared.locked().is_some()
    }

    /// The requester holding the lease, if one is.
    #[must_use]
    pub fn holder(&self) -> Option<String> {
        self.shared
            .locked()
            .as_ref()
            .map(|held| held.reason.requester.clone())
    }

    /// Reclaims the lease if its deadline has passed, returning the diagnostic raised.
    ///
    /// Public because the timer should not be the only thing able to notice: a
    /// supervisor loop, a render tick, or the next `acquire` can drive it too.
    pub fn reclaim_if_expired(&self) -> Option<ForcedReclaim> {
        self.shared.reclaim_if_expired()
    }

    async fn acquire_inner(
        &self,
        reason: LeaseReason,
        cleanup: Option<Arc<dyn TerminalLeaseCleanup>>,
    ) -> Result<TerminalLeaseGuard, TerminalLeaseError> {
        self.shared.reclaim_if_expired();

        if let Some(held) = self.shared.locked().as_ref() {
            return Err(TerminalLeaseError::Busy {
                holder: held.reason.requester.clone(),
                holder_purpose: held.reason.purpose.clone(),
                requested_by: reason.requester.clone(),
            });
        }

        self.shared
            .owner
            .yield_terminal(&reason)
            .await
            .map_err(|detail| TerminalLeaseError::Unavailable {
                requested_by: reason.requester.clone(),
                detail,
            })?;

        let settled = Arc::new(AtomicBool::new(false));
        {
            let mut slot = self.shared.locked();
            if let Some(held) = slot.as_ref() {
                let error = TerminalLeaseError::Busy {
                    holder: held.reason.requester.clone(),
                    holder_purpose: held.reason.purpose.clone(),
                    requested_by: reason.requester.clone(),
                };
                drop(slot);
                self.shared
                    .owner
                    .reclaim_terminal(&reason, ReclaimCause::Released);
                return Err(error);
            }
            *slot = Some(Held {
                reason: reason.clone(),
                deadline: Instant::now() + self.timeout,
                timeout: self.timeout,
                settled: Arc::clone(&settled),
                cleanup: cleanup.clone(),
            });
        }

        let (released_tx, released_rx) = oneshot::channel::<()>();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let shared = Arc::clone(&self.shared);
            let timeout = self.timeout;
            handle.spawn(async move {
                tokio::select! {
                    () = tokio::time::sleep(timeout) => {
                        let _finished = tokio::task::spawn_blocking(move || {
                            shared.reclaim_if_expired()
                        }).await;
                    }
                    _ = released_rx => {}
                }
            });
        }

        Ok(TerminalLeaseGuard {
            shared: Arc::clone(&self.shared),
            reason,
            settled,
            cleanup,
            _released: released_tx,
        })
    }
}

#[async_trait]
impl TerminalLease for TerminalBroker {
    async fn acquire(&self, reason: LeaseReason) -> Result<TerminalLeaseGuard, TerminalLeaseError> {
        self.acquire_inner(reason, None).await
    }

    async fn acquire_with_cleanup(
        &self,
        reason: LeaseReason,
        cleanup: Arc<dyn TerminalLeaseCleanup>,
    ) -> Result<TerminalLeaseGuard, TerminalLeaseError> {
        self.acquire_inner(reason, Some(cleanup)).await
    }
}

/// A held terminal lease. Releasing it is dropping it.
///
/// Deliberately not `Clone`: two handles to one exclusive lease would make "which one
/// releases it" a question whose only answer is refcounting, and that is a second
/// exclusion mechanism competing with the broker's.
pub struct TerminalLeaseGuard {
    shared: Arc<SharedSlot>,
    reason: LeaseReason,
    settled: Arc<AtomicBool>,
    cleanup: Option<Arc<dyn TerminalLeaseCleanup>>,
    /// Dropped with the guard, which is what stops the watchdog. Never read.
    _released: oneshot::Sender<()>,
}

impl TerminalLeaseGuard {
    /// Who this lease was granted to.
    #[must_use]
    pub fn reason(&self) -> &LeaseReason {
        &self.reason
    }

    /// Whether the deadline already took this lease away.
    ///
    /// A host returning from a long prompt can ask before writing anything: the
    /// terminal may belong to the TUI again, and output written now would land in the
    /// middle of a redraw.
    #[must_use]
    pub fn was_reclaimed(&self) -> bool {
        self.settled.load(Ordering::SeqCst)
    }

    /// Returns the terminal now rather than at end of scope.
    ///
    /// Sugar over `drop`, present so host code can be explicit at the point the prompt
    /// finishes instead of relying on a reader spotting where the binding dies.
    pub fn release(self) {
        drop(self);
    }
}

impl fmt::Debug for TerminalLeaseGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TerminalLeaseGuard")
            .field("requester", &self.reason.requester)
            .field("purpose", &self.reason.purpose)
            .field("reclaimed", &self.was_reclaimed())
            .finish_non_exhaustive()
    }
}

/// Reclaim is synchronous, including a cleanup hook that blocks.
///
/// `before_reclaim` reaps the holder's child, and the TUI's implementation waits for a
/// terminating editor for up to its reap timeout. On a current-thread runtime that
/// blocks the reactor for the same duration, and the offload that would avoid it is
/// available here — `Handle::try_current()` plus `spawn_blocking`, exactly as the
/// deadline sweep uses 90 lines above. It is deliberately not used, for three reasons
/// that all outrank the stall:
///
/// * The terminal would be returned to its owner *later* than `release` returns, and
///   [`TerminalLeaseGuard::release`] promises the opposite. A TUI that redraws on the
///   next line would then paint over a child that still holds the TTY.
/// * The next `acquire` would see the slot still occupied and fail with
///   [`TerminalLeaseError::Busy`] for a lease its predecessor had already given up.
/// * `_released` is dropped with the guard, so the watchdog that would otherwise
///   force-reclaim stops at exactly the moment the offloaded work begins. If the runtime
///   shuts down before that task is polled, nothing reclaims the terminal at all and the
///   user is left in raw mode. Failing closed here means blocking, not deferring.
///
/// `lease_release_reclaims_the_terminal_before_it_returns` pins this; the stall is only
/// reachable when a child is still alive, which for the TUI means the editor was
/// cancelled rather than closed.
impl Drop for TerminalLeaseGuard {
    fn drop(&mut self) {
        if self.settled.load(Ordering::SeqCst) {
            return;
        }
        if let Some(cleanup) = &self.cleanup
            && cleanup.before_reclaim().is_err()
        {
            return;
        }
        // Losing the settle means the deadline already force-reclaimed this lease.
        // Reclaiming again would restore a terminal the owner has since redrawn.
        if !self.shared.settle(&self.settled) {
            return;
        }
        self.shared
            .owner
            .reclaim_terminal(&self.reason, ReclaimCause::Released);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The smallest owner that can prove ordering: an append-only transition log.
    /// `zuno_testkit::FakeTerminalOwner` is the same idea plus the waiting helpers the
    /// requester wave needs; this one stays here so the protocol's own tests do not
    /// depend on a crate above it.
    struct RecordingOwner {
        log: Arc<Mutex<Vec<String>>>,
        refuse: Option<String>,
    }

    impl RecordingOwner {
        fn new() -> (Arc<Self>, Arc<Mutex<Vec<String>>>) {
            let log = Arc::new(Mutex::new(Vec::new()));
            (
                Arc::new(Self {
                    log: Arc::clone(&log),
                    refuse: None,
                }),
                log,
            )
        }

        fn refusing(detail: &str) -> (Arc<Self>, Arc<Mutex<Vec<String>>>) {
            let log = Arc::new(Mutex::new(Vec::new()));
            (
                Arc::new(Self {
                    log: Arc::clone(&log),
                    refuse: Some(detail.to_owned()),
                }),
                log,
            )
        }
    }

    #[async_trait]
    impl TerminalOwner for RecordingOwner {
        async fn yield_terminal(&self, reason: &LeaseReason) -> Result<(), String> {
            if let Some(detail) = &self.refuse {
                self.log
                    .lock()
                    .unwrap()
                    .push(format!("refused {}", reason.requester));
                return Err(detail.clone());
            }
            self.log
                .lock()
                .unwrap()
                .push(format!("yield {}", reason.requester));
            Ok(())
        }

        fn reclaim_terminal(&self, reason: &LeaseReason, cause: ReclaimCause) {
            let tag = match cause {
                ReclaimCause::Released => "reclaim-released".to_owned(),
                ReclaimCause::Deadline(forced) => format!("reclaim-forced {forced}"),
            };
            self.log
                .lock()
                .unwrap()
                .push(format!("{tag} {}", reason.requester));
        }
    }

    fn entries(log: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        log.lock().unwrap().clone()
    }

    fn reclaims(log: &Arc<Mutex<Vec<String>>>) -> usize {
        entries(log)
            .iter()
            .filter(|line| line.starts_with("reclaim"))
            .count()
    }

    struct RecordingCleanup {
        log: Arc<Mutex<Vec<String>>>,
    }

    impl TerminalLeaseCleanup for RecordingCleanup {
        fn before_reclaim(&self) -> Result<(), String> {
            self.log.lock().unwrap().push(String::from("cleanup"));
            Ok(())
        }
    }

    /// A deadline no test run can reach, for the assertions that must observe the
    /// *absence* of a force-reclaim. Load can only delay a timer, so this direction
    /// cannot flake.
    const NEVER: Duration = Duration::from_secs(3_600);

    #[tokio::test]
    async fn lease_yields_then_reclaims_in_that_order() {
        let (owner, log) = RecordingOwner::new();
        let broker = TerminalBroker::with_timeout(owner, NEVER);

        let guard = broker
            .acquire(LeaseReason::new("kiro", "device-code prompt"))
            .await
            .expect("a vacant terminal must be grantable");
        assert_eq!(entries(&log), vec!["yield kiro".to_owned()]);
        assert!(broker.is_held());

        guard.release();
        assert_eq!(
            entries(&log),
            vec!["yield kiro".to_owned(), "reclaim-released kiro".to_owned()]
        );
        assert!(!broker.is_held());
    }

    #[tokio::test]
    async fn lease_refuses_a_second_holder_and_names_the_first() {
        let (owner, log) = RecordingOwner::new();
        let broker = TerminalBroker::with_timeout(owner, NEVER);
        let first = broker
            .acquire(LeaseReason::new("kiro", "device-code prompt"))
            .await
            .expect("first acquire");

        let error = broker
            .acquire(LeaseReason::new("other", "api key prompt"))
            .await
            .expect_err("the declared policy is refusal, not queueing");

        assert_eq!(
            error,
            TerminalLeaseError::Busy {
                holder: "kiro".to_owned(),
                holder_purpose: "device-code prompt".to_owned(),
                requested_by: "other".to_owned(),
            }
        );
        // A refusal must not disturb the holder.
        assert_eq!(entries(&log), vec!["yield kiro".to_owned()]);
        drop(first);
        broker
            .acquire(LeaseReason::new("other", "api key prompt"))
            .await
            .expect("the terminal is grantable again once released");
    }

    #[tokio::test]
    async fn lease_owner_refusal_leaves_the_slot_vacant() {
        let (owner, _log) = RecordingOwner::refusing("no tty on this stdio session");
        let broker = TerminalBroker::with_timeout(owner, NEVER);

        let error = broker
            .acquire(LeaseReason::new("kiro", "device-code prompt"))
            .await
            .expect_err("an owner that cannot yield must refuse the lease");

        assert!(matches!(
            error,
            TerminalLeaseError::Unavailable { ref detail, .. } if detail.contains("no tty")
        ));
        assert!(
            !broker.is_held(),
            "a refused lease must not occupy the slot"
        );
    }

    #[tokio::test]
    async fn lease_sweep_reclaims_an_expired_holder_without_a_timer() {
        let (owner, log) = RecordingOwner::new();
        let broker = TerminalBroker::with_timeout(owner, Duration::ZERO);

        let leaked = broker
            .acquire(LeaseReason::new("kiro", "device-code prompt"))
            .await
            .expect("first acquire");

        // A zero deadline has already passed on return, so the next acquire must sweep
        // rather than refuse. No sleeping and no timer: fully deterministic.
        let second = broker
            .acquire(LeaseReason::new("other", "api key prompt"))
            .await
            .expect("an expired holder must not wedge the terminal");

        let log_lines = entries(&log);
        assert!(
            log_lines
                .iter()
                .any(|line| line.starts_with("reclaim-forced")
                    && line.contains("requester `kiro`")
                    && line.contains("did not release it")),
            "the force-reclaim diagnostic must name the requester: {log_lines:?}"
        );
        assert!(leaked.was_reclaimed());
        drop(leaked);
        assert_eq!(
            reclaims(&log),
            1,
            "an already-settled guard must not reclaim on top of the new holder"
        );
        drop(second);
    }

    #[tokio::test]
    async fn lease_deadline_runs_registered_cleanup_before_reclaiming_the_terminal() {
        let (owner, log) = RecordingOwner::new();
        let broker = TerminalBroker::with_timeout(owner, Duration::ZERO);
        let cleanup: Arc<dyn TerminalLeaseCleanup> = Arc::new(RecordingCleanup {
            log: Arc::clone(&log),
        });
        let guard = broker
            .acquire_with_cleanup(LeaseReason::new("tui", "external editor"), cleanup)
            .await
            .expect("acquire");

        broker
            .reclaim_if_expired()
            .expect("the elapsed lease is reclaimed");

        let lines = entries(&log);
        assert_eq!(lines[0], "yield tui");
        assert_eq!(lines[1], "cleanup");
        assert!(
            lines[2].starts_with("reclaim-forced"),
            "the terminal was reclaimed before cleanup: {lines:?}"
        );
        drop(guard);
    }

    /// A cleanup that blocks the way reaping a terminating child blocks, and records
    /// the thread it ran on.
    struct BlockingCleanup {
        log: Arc<Mutex<Vec<String>>>,
        thread: Arc<Mutex<Option<std::thread::ThreadId>>>,
    }

    impl TerminalLeaseCleanup for BlockingCleanup {
        fn before_reclaim(&self) -> Result<(), String> {
            *self.thread.lock().unwrap() = Some(std::thread::current().id());
            std::thread::sleep(Duration::from_millis(20));
            self.log.lock().unwrap().push(String::from("cleanup"));
            Ok(())
        }
    }

    /// Releasing a lease has already reclaimed the terminal by the time it returns.
    ///
    /// The reactor stall inside `before_reclaim` is the price of this, and this test is
    /// what makes that a decision instead of an oversight: moving the cleanup onto
    /// `spawn_blocking` would still satisfy "the cleanup eventually runs", and it cannot
    /// satisfy this. The assertions run with no intervening await, so they observe only
    /// what `release` finished doing, and the recorded thread is the releasing one — a
    /// cleanup that ran anywhere else means the child was still holding the TTY when the
    /// owner was told it had it back.
    #[tokio::test]
    async fn lease_release_reclaims_the_terminal_before_it_returns() {
        let (owner, log) = RecordingOwner::new();
        let broker = TerminalBroker::with_timeout(owner, NEVER);
        let thread = Arc::new(Mutex::new(None));
        let cleanup: Arc<dyn TerminalLeaseCleanup> = Arc::new(BlockingCleanup {
            log: Arc::clone(&log),
            thread: Arc::clone(&thread),
        });
        let guard = broker
            .acquire_with_cleanup(LeaseReason::new("tui", "external editor"), cleanup)
            .await
            .expect("acquire");
        assert!(
            tokio::runtime::Handle::try_current().is_ok(),
            "the offload this test rules out is available on this runtime"
        );

        let releasing = std::thread::current().id();
        guard.release();

        assert_eq!(
            entries(&log),
            vec![
                "yield tui".to_owned(),
                "cleanup".to_owned(),
                "reclaim-released tui".to_owned()
            ],
            "release returned before the terminal was back with its owner"
        );
        assert_eq!(
            *thread.lock().unwrap(),
            Some(releasing),
            "the child was reaped on another thread, so release could only promise the \
             terminal was returned"
        );
        assert!(
            broker.reclaim_if_expired().is_none(),
            "the released lease is settled, so nothing is left for the watchdog"
        );
    }

    #[tokio::test]
    async fn lease_reclaim_runs_exactly_once_per_grant() {
        let (owner, log) = RecordingOwner::new();
        let broker = TerminalBroker::with_timeout(owner, NEVER);

        let guard = broker
            .acquire(LeaseReason::new("kiro", "device-code prompt"))
            .await
            .expect("acquire");
        drop(guard);
        assert!(broker.reclaim_if_expired().is_none());

        assert_eq!(reclaims(&log), 1);
    }

    #[tokio::test]
    async fn lease_guard_debug_reports_the_plugin() {
        let (owner, _log) = RecordingOwner::new();
        let broker = TerminalBroker::with_timeout(owner, NEVER);
        let guard = broker
            .acquire(LeaseReason::new("kiro", "device-code prompt"))
            .await
            .expect("acquire");

        let rendered = format!("{guard:?}");
        assert!(rendered.contains("kiro"), "{rendered}");
        assert!(rendered.contains("reclaimed: false"), "{rendered}");
        assert_eq!(broker.holder().as_deref(), Some("kiro"));
        assert_eq!(broker.timeout(), NEVER);
        assert_eq!(
            guard.reason().to_string(),
            "requester `kiro` (device-code prompt)"
        );
    }

    #[test]
    fn lease_default_timeout_is_sized_for_a_human_typing_a_code() {
        assert_eq!(DEFAULT_LEASE_TIMEOUT, Duration::from_secs(300));
        assert!(!ReclaimCause::Released.is_forced());
        assert!(
            ReclaimCause::Deadline(ForcedReclaim {
                requester: "kiro".to_owned(),
                purpose: "device-code prompt".to_owned(),
                timeout: Duration::from_millis(25),
            })
            .is_forced()
        );
    }

    /// The default constructor is the one production uses; a test that never builds it
    /// would let a wrong default ship.
    #[tokio::test]
    async fn lease_default_broker_grants_with_the_production_deadline() {
        let (owner, log) = RecordingOwner::new();
        let broker = TerminalBroker::new(owner);
        assert_eq!(broker.timeout(), DEFAULT_LEASE_TIMEOUT);

        let guard = broker
            .acquire(LeaseReason::new("kiro", "device-code prompt"))
            .await
            .expect("acquire");
        assert!(!guard.was_reclaimed());
        drop(guard);
        assert_eq!(reclaims(&log), 1);
    }
}

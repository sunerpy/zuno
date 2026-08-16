//! Exclusive, time-bounded ownership of the one shared terminal.
//!
//! # The deadlock this exists to prevent
//!
//! A plugin's compat host is a `bun`/`node` child process, and real plugins prompt
//! on stdin: kiro@0.18.0 uses `node:readline/promises` for its interactive OAuth
//! code entry. Meanwhile the Rust TUI holds the same TTY in raw mode inside the
//! alternate screen, consuming every keystroke. Run both at once and the child waits
//! forever for a line it will never receive while the user's typing is eaten by a
//! render loop. That is blocker B7/B11: a runtime deadlock class, not a rendering
//! glitch.
//!
//! The fix is a lease. Exactly one party may hold the terminal; a host asks for it,
//! the owner suspends and yields the TTY, the host prompts, and the lease is
//! returned. Every way a host can touch the terminal is expressed as "hold a lease",
//! so the failure mode becomes a refusal or a reclaim carrying a diagnostic instead
//! of a hang.
//!
//! # Why the protocol lives here and not in `zuno-tui`
//!
//! Both sides speak it: `zuno-plugin` acquires, `zuno-tui` grants. Putting it in
//! `zuno-tui` would force `zuno-plugin -> zuno-tui`, i.e. the plugin host would depend on
//! ratatui in order to ask for stdin. `zuno-engine` is already below `zuno-plugin`, and
//! `zuno-tui` can depend on it when todo 73 implements the real owner, so this crate
//! is the only place the edge points the right way for both. Same reasoning as
//! [`zuno_tool::InterruptHandle`]: the lower crate names the operations, the higher
//! crate supplies the implementation.
//!
//! A consequence worth stating: **nothing here touches a terminal.** No `crossterm`,
//! no ioctl, no `isatty`. This module is the state machine and the vocabulary; the
//! two physical transitions belong to [`TerminalOwner`], and todo 73 implements them
//! over ratatui. That is what lets the plugin wave prove its half against
//! `zuno_testkit::FakeTerminalOwner` with no TTY in the picture at all.
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
/// enough that a wedged plugin does not strand the session. Override with
/// [`TerminalBroker::with_timeout`]; tests must, so that no test waits a production
/// interval.
pub const DEFAULT_LEASE_TIMEOUT: Duration = Duration::from_secs(300);

/// Why a host wants the terminal, and on whose behalf.
///
/// The plugin name is carried separately from the human-readable purpose because the
/// force-reclaim diagnostic has to name a culprit. "A lease expired" is not
/// actionable; "plugin `kiro` did not release it" is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseReason {
    /// The plugin the lease is granted to. Appears in every diagnostic.
    pub plugin: String,
    /// What it is about to do, phrased for a user who sees the TUI step aside.
    pub purpose: String,
}

impl LeaseReason {
    /// A reason naming the plugin and its purpose.
    #[must_use]
    pub fn new(plugin: impl Into<String>, purpose: impl Into<String>) -> Self {
        Self {
            plugin: plugin.into(),
            purpose: purpose.into(),
        }
    }
}

impl fmt::Display for LeaseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "plugin `{}` ({})", self.plugin, self.purpose)
    }
}

/// A refused lease.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TerminalLeaseError {
    /// Someone else holds the terminal and their deadline has not passed.
    ///
    /// Refusal, not queueing — see [`TerminalBroker`] for the argument.
    #[error(
        "the terminal is held by plugin `{holder}` ({holder_purpose}); \
         plugin `{requested_by}` cannot prompt until it is released"
    )]
    Busy {
        /// The plugin currently holding the lease.
        holder: String,
        /// What the holder said it was doing.
        holder_purpose: String,
        /// The plugin that was refused.
        requested_by: String,
    },

    /// The owner could not give the terminal up.
    ///
    /// Distinct from [`Self::Busy`]: nobody holds the lease, the terminal itself is
    /// unavailable — no TTY, a render loop that will not stop, a mode restore that
    /// failed. The host must not prompt.
    #[error("plugin `{requested_by}` was not given the terminal: {detail}")]
    Unavailable {
        /// The plugin that asked.
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
    /// The plugin that failed to release. The point of the whole type.
    pub plugin: String,
    /// What the plugin said it was doing when it took the lease.
    pub purpose: String,
    /// The deadline it blew.
    pub timeout: Duration,
}

impl fmt::Display for ForcedReclaim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "plugin `{}` held the terminal for `{}` past its {} ms deadline \
             and did not release it; the terminal was reclaimed by force",
            self.plugin,
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
}

/// The lease that is currently out, if any.
#[derive(Debug)]
struct Held {
    reason: LeaseReason,
    /// When the deadline passes. Compared against [`Instant::now`], so a loaded
    /// machine can only be late, never early.
    deadline: Instant,
    timeout: Duration,
    /// Set by whichever of release-or-reclaim gets there first. The loser then does
    /// nothing, which is what makes `reclaim_terminal` exactly-once under a race.
    settled: Arc<AtomicBool>,
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
        let mut slot = self.locked();
        if slot
            .as_ref()
            .is_some_and(|held| Arc::ptr_eq(&held.settled, settled))
        {
            slot.take();
        }
        won
    }

    /// Removes an expired holder, returning what has to be reclaimed for it.
    ///
    /// Split from the reclaim itself so that `reclaim_terminal` — arbitrary owner code
    /// that writes to a terminal — never runs while this mutex is held. A panic in
    /// there would otherwise poison the slot for every later lease.
    fn take_expired(&self, now: Instant) -> Option<(LeaseReason, ForcedReclaim)> {
        let mut slot = self.locked();
        let won = match slot.as_ref() {
            Some(held) if now >= held.deadline => !held.settled.swap(true, Ordering::SeqCst),
            _ => return None,
        };
        let held = slot.take();
        drop(slot);
        if !won {
            return None;
        }
        let held = held?;
        let diagnostic = ForcedReclaim {
            plugin: held.reason.plugin.clone(),
            purpose: held.reason.purpose.clone(),
            timeout: held.timeout,
        };
        Some((held.reason, diagnostic))
    }

    /// Reclaims an expired lease. Returns the diagnostic raised, if one was.
    fn reclaim_if_expired(&self) -> Option<ForcedReclaim> {
        let (reason, diagnostic) = self.take_expired(Instant::now())?;
        self.owner
            .reclaim_terminal(&reason, ReclaimCause::Deadline(diagnostic.clone()));
        Some(diagnostic)
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
///    host say "another plugin is prompting" *now*.
/// 2. **Queueing hides the deadlock this module removes.** A host that never releases
///    would make every later acquirer block, so the symptom would again be a hang —
///    one level further out — and the force-reclaim would only free the head of the
///    queue. A refusal is observable at the call site, which is where a plugin author
///    can act on it.
/// 3. **Preemption corrupts the thing being protected.** Revoking a lease mid-prompt
///    yanks the terminal out from under half-typed input. Preemption is reserved for
///    the one case where the holder has already broken the contract: the deadline.
///
/// So the only involuntary transition is the deadline, and it is loud.
///
/// # Two paths reclaim an expired lease, and that is deliberate
///
/// A watchdog task fires at the deadline, so a wedged plugin is reclaimed even when
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

    /// The plugin holding the lease, if one is.
    #[must_use]
    pub fn holder(&self) -> Option<String> {
        self.shared
            .locked()
            .as_ref()
            .map(|held| held.reason.plugin.clone())
    }

    /// Reclaims the lease if its deadline has passed, returning the diagnostic raised.
    ///
    /// Public because the timer should not be the only thing able to notice: a
    /// supervisor loop, a render tick, or the next `acquire` can drive it too.
    pub fn reclaim_if_expired(&self) -> Option<ForcedReclaim> {
        self.shared.reclaim_if_expired()
    }
}

#[async_trait]
impl TerminalLease for TerminalBroker {
    async fn acquire(&self, reason: LeaseReason) -> Result<TerminalLeaseGuard, TerminalLeaseError> {
        // Sweep first: an expired holder is not a holder. Doing this before the busy
        // check is what stops one leaked guard from wedging the terminal for the rest
        // of the session.
        self.shared.reclaim_if_expired();

        // Decide under the lock, but do not reserve the slot yet: `yield_terminal` may
        // refuse, and a reserved-then-released slot would let a concurrent acquire see
        // a holder that never existed.
        if let Some(held) = self.shared.locked().as_ref() {
            return Err(TerminalLeaseError::Busy {
                holder: held.reason.plugin.clone(),
                holder_purpose: held.reason.purpose.clone(),
                requested_by: reason.plugin.clone(),
            });
        }

        self.shared
            .owner
            .yield_terminal(&reason)
            .await
            .map_err(|detail| TerminalLeaseError::Unavailable {
                requested_by: reason.plugin.clone(),
                detail,
            })?;

        let settled = Arc::new(AtomicBool::new(false));
        {
            let mut slot = self.shared.locked();
            if let Some(held) = slot.as_ref() {
                // Another task took the terminal while this one awaited the owner. The
                // terminal is already yielded, so hand it straight back rather than
                // leaving it in a state nobody owns.
                let error = TerminalLeaseError::Busy {
                    holder: held.reason.plugin.clone(),
                    holder_purpose: held.reason.purpose.clone(),
                    requested_by: reason.plugin.clone(),
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
            });
        }

        // The watchdog. `released_rx` resolves the moment the guard is dropped, because
        // the guard owns the sender — including on a panic unwind, which a
        // notification-based scheme would miss. There is no lost-wakeup window either:
        // a dropped sender leaves the channel permanently closed rather than depending
        // on a waiter having registered first.
        //
        // `try_current` rather than an unconditional spawn: `acquire` is async, but an
        // executor other than Tokio can poll it, and panicking inside the terminal
        // protocol is a worse outcome than degrading to sweep-only reclaim. When there
        // is no Tokio runtime the deadline is enforced by the next `acquire` or by
        // `reclaim_if_expired`.
        let (released_tx, released_rx) = oneshot::channel::<()>();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let shared = Arc::clone(&self.shared);
            let timeout = self.timeout;
            handle.spawn(async move {
                tokio::select! {
                    () = tokio::time::sleep(timeout) => {
                        shared.reclaim_if_expired();
                    }
                    _ = released_rx => {}
                }
            });
        }

        Ok(TerminalLeaseGuard {
            shared: Arc::clone(&self.shared),
            reason,
            settled,
            _released: released_tx,
        })
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
            .field("plugin", &self.reason.plugin)
            .field("purpose", &self.reason.purpose)
            .field("reclaimed", &self.was_reclaimed())
            .finish_non_exhaustive()
    }
}

impl Drop for TerminalLeaseGuard {
    fn drop(&mut self) {
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
    /// plugin wave needs; this one stays here so the protocol's own tests do not
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
                    .push(format!("refused {}", reason.plugin));
                return Err(detail.clone());
            }
            self.log
                .lock()
                .unwrap()
                .push(format!("yield {}", reason.plugin));
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
                .push(format!("{tag} {}", reason.plugin));
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
                    && line.contains("plugin `kiro`")
                    && line.contains("did not release it")),
            "the force-reclaim diagnostic must name the plugin: {log_lines:?}"
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
            "plugin `kiro` (device-code prompt)"
        );
    }

    #[test]
    fn lease_default_timeout_is_sized_for_a_human_typing_a_code() {
        assert_eq!(DEFAULT_LEASE_TIMEOUT, Duration::from_secs(300));
        assert!(!ReclaimCause::Released.is_forced());
        assert!(
            ReclaimCause::Deadline(ForcedReclaim {
                plugin: "kiro".to_owned(),
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

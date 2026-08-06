//! A [`TerminalOwner`] that owns no terminal, for proving the lease protocol.
//!
//! # Why a fake is the right instrument here, not a shortcut
//!
//! The lease exists because a plugin's `readline` prompt and a ratatui render loop
//! cannot share one TTY (blocker B7). The plugin side of that protocol has to be
//! provable *now*, and the TUI that implements the real owner arrives thirteen todos
//! later. Even once it exists, driving it would mean a pty, a real raw-mode
//! transition, and a test that cannot run on a machine with no controlling terminal.
//!
//! So this records the transitions instead. It observes exactly what the protocol
//! promises — a yield before every grant, a reclaim after every lease, and whether
//! that reclaim was orderly or forced — and nothing about how a terminal is drawn.
//! The real ratatui integration, over a real pty, is todo 73's job and is a different
//! kind of test.
//!
//! # What it does not prove
//!
//! That suspending ratatui actually restores cooked mode, that a `bun` child really
//! sees the freed TTY, or that the reclaim redraws correctly. Those need a terminal.
//! Read a green run here as "the protocol's ordering, exclusion and deadline hold",
//! not as "the terminal works".
//!
//! ```no_run
//! use std::sync::Arc;
//! use std::time::Duration;
//! use oc_engine::terminal_lease::{LeaseReason, TerminalBroker, TerminalLease};
//! use oc_testkit::FakeTerminalOwner;
//!
//! # async fn example() {
//! let owner = Arc::new(FakeTerminalOwner::new());
//! let transcript = owner.transcript();
//! let broker = TerminalBroker::with_timeout(owner, Duration::from_secs(3600));
//!
//! let guard = broker
//!     .acquire(LeaseReason::new("kiro", "device-code prompt"))
//!     .await
//!     .expect("a vacant terminal");
//! // ... the host prompts on stdin here ...
//! guard.release();
//!
//! assert!(transcript.released_by("kiro"));
//! # }
//! ```

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use oc_engine::terminal_lease::{LeaseReason, ReclaimCause, TerminalOwner};

/// One observed transition of terminal ownership.
///
/// Deliberately three variants and not two-plus-a-flag: a test that must prove a
/// force-reclaim happened should not be able to pass by matching a `Released` whose
/// boolean it forgot to check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalTransition {
    /// The owner gave the terminal up and the lease was granted.
    Acquired {
        /// The plugin the lease went to.
        plugin: String,
        /// What it said it was doing.
        purpose: String,
    },
    /// The owner refused to give the terminal up, so no lease was granted.
    Refused {
        /// The plugin that asked.
        plugin: String,
        /// The reason handed back to it.
        detail: String,
    },
    /// The guard was dropped and the terminal came back in order.
    Released {
        /// The plugin that returned it.
        plugin: String,
    },
    /// The deadline passed and the terminal was taken back.
    ForceReclaimed {
        /// The plugin that failed to release.
        plugin: String,
        /// The rendered diagnostic, exactly as a user would read it.
        ///
        /// Stored rendered so a test asserts on the sentence that will actually be
        /// shown, rather than on fields a `Display` impl might later drop.
        diagnostic: String,
        /// The deadline that was blown.
        timeout: Duration,
    },
}

impl TerminalTransition {
    /// The plugin this transition concerns.
    #[must_use]
    pub fn plugin(&self) -> &str {
        match self {
            Self::Acquired { plugin, .. }
            | Self::Refused { plugin, .. }
            | Self::Released { plugin }
            | Self::ForceReclaimed { plugin, .. } => plugin,
        }
    }
}

/// A read-only, cheaply cloneable view of one [`FakeTerminalOwner`]'s log.
///
/// The broker takes the owner by `Arc<dyn TerminalOwner>` and a test still has to
/// read what happened, so the log is shared rather than owned. Handing out a
/// transcript instead of the owner also keeps a test from calling
/// `yield_terminal`/`reclaim_terminal` directly and "proving" an ordering the broker
/// never produced.
#[derive(Debug, Clone)]
pub struct TerminalTranscript {
    entries: Arc<Mutex<Vec<TerminalTransition>>>,
}

impl TerminalTranscript {
    fn locked(&self) -> MutexGuard<'_, Vec<TerminalTransition>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Every transition observed so far, oldest first.
    #[must_use]
    pub fn transitions(&self) -> Vec<TerminalTransition> {
        self.locked().clone()
    }

    /// How many transitions have been observed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.locked().len()
    }

    /// Whether nothing has happened yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.locked().is_empty()
    }

    /// Whether `plugin` was granted the terminal at least once.
    #[must_use]
    pub fn acquired_by(&self, plugin: &str) -> bool {
        self.transitions().iter().any(
            |transition| matches!(transition, TerminalTransition::Acquired { plugin: p, .. } if p == plugin),
        )
    }

    /// Whether `plugin` returned the terminal in order at least once.
    #[must_use]
    pub fn released_by(&self, plugin: &str) -> bool {
        self.transitions().iter().any(
            |transition| matches!(transition, TerminalTransition::Released { plugin: p } if p == plugin),
        )
    }

    /// The diagnostic of the first force-reclaim recorded against `plugin`.
    ///
    /// `Option` rather than a bool so the test asserts on the sentence, which is what
    /// the QA failure scenario is about: the diagnostic has to name the plugin.
    #[must_use]
    pub fn forced_diagnostic(&self, plugin: &str) -> Option<String> {
        self.transitions()
            .into_iter()
            .find_map(|transition| match transition {
                TerminalTransition::ForceReclaimed {
                    plugin: p,
                    diagnostic,
                    ..
                } if p == plugin => Some(diagnostic),
                _ => None,
            })
    }

    /// How many force-reclaims have been recorded.
    #[must_use]
    pub fn forced_count(&self) -> usize {
        self.transitions()
            .iter()
            .filter(|transition| matches!(transition, TerminalTransition::ForceReclaimed { .. }))
            .count()
    }

    /// How many times the terminal has changed hands in either direction.
    ///
    /// The invariant a caller usually wants: every `Acquired` is followed by exactly
    /// one `Released` or `ForceReclaimed`, so this must be even once quiescent.
    #[must_use]
    pub fn ownership_changes(&self) -> usize {
        self.transitions()
            .iter()
            .filter(|transition| !matches!(transition, TerminalTransition::Refused { .. }))
            .count()
    }

    /// Waits until `predicate` accepts the log, or the budget runs out.
    ///
    /// Bounded polling against a deadline, never a bare sleep sized to a timeout.
    /// A sleep long enough to be reliable on a loaded machine is a slow test, and one
    /// short enough to be fast is a flake; polling for an *observation* is neither,
    /// because extra load can only make the wait longer, not make the assertion wrong.
    ///
    /// Returns `true` if the predicate was satisfied within `budget`.
    pub async fn wait_until(
        &self,
        budget: Duration,
        predicate: impl Fn(&[TerminalTransition]) -> bool,
    ) -> bool {
        let deadline = Instant::now() + budget;
        loop {
            if predicate(&self.transitions()) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    /// Waits for a force-reclaim naming `plugin`, returning its diagnostic.
    ///
    /// The shape the timeout test needs: assert that the reclaim *happens*, with a
    /// budget generous enough that load cannot turn a late timer into a failure.
    pub async fn wait_for_forced(&self, plugin: &str, budget: Duration) -> Option<String> {
        self.wait_until(budget, |transitions| {
            transitions.iter().any(|transition| {
                matches!(transition, TerminalTransition::ForceReclaimed { plugin: p, .. } if p == plugin)
            })
        })
        .await;
        self.forced_diagnostic(plugin)
    }
}

/// A terminal owner that records transitions instead of driving a terminal.
///
/// Construct it, take a [`TerminalTranscript`] before handing it to a
/// [`oc_engine::terminal_lease::TerminalBroker`], then assert on the transcript.
pub struct FakeTerminalOwner {
    transcript: TerminalTranscript,
    /// When set, `yield_terminal` refuses with this detail.
    refuse_with: Option<String>,
    /// How many times the owner was asked to yield, refusals included.
    ///
    /// Separate from the transcript so a test can prove the broker consulted the owner
    /// even on a path that records nothing.
    yields_requested: AtomicUsize,
}

impl FakeTerminalOwner {
    /// An owner that always yields.
    #[must_use]
    pub fn new() -> Self {
        Self {
            transcript: TerminalTranscript {
                entries: Arc::new(Mutex::new(Vec::new())),
            },
            refuse_with: None,
            yields_requested: AtomicUsize::new(0),
        }
    }

    /// An owner that refuses every request, e.g. a session with no TTY.
    ///
    /// The protocol has to have an answer for "the terminal cannot be yielded" that is
    /// not the same answer as "someone else has it", because a host must not prompt in
    /// either case but only one of them is worth retrying.
    #[must_use]
    pub fn refusing(detail: impl Into<String>) -> Self {
        Self {
            refuse_with: Some(detail.into()),
            ..Self::new()
        }
    }

    /// A shared, read-only view of what this owner has observed.
    #[must_use]
    pub fn transcript(&self) -> TerminalTranscript {
        self.transcript.clone()
    }

    /// How many times the broker asked this owner to yield, refusals included.
    #[must_use]
    pub fn yields_requested(&self) -> usize {
        self.yields_requested.load(Ordering::SeqCst)
    }

    fn record(&self, transition: TerminalTransition) {
        self.transcript.locked().push(transition);
    }
}

impl Default for FakeTerminalOwner {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for FakeTerminalOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeTerminalOwner")
            .field("refusing", &self.refuse_with.is_some())
            .field("yields_requested", &self.yields_requested())
            .field("transitions", &self.transcript.len())
            .finish()
    }
}

#[async_trait]
impl TerminalOwner for FakeTerminalOwner {
    async fn yield_terminal(&self, reason: &LeaseReason) -> Result<(), String> {
        self.yields_requested.fetch_add(1, Ordering::SeqCst);
        if let Some(detail) = &self.refuse_with {
            self.record(TerminalTransition::Refused {
                plugin: reason.plugin.clone(),
                detail: detail.clone(),
            });
            return Err(detail.clone());
        }
        self.record(TerminalTransition::Acquired {
            plugin: reason.plugin.clone(),
            purpose: reason.purpose.clone(),
        });
        Ok(())
    }

    fn reclaim_terminal(&self, reason: &LeaseReason, cause: ReclaimCause) {
        let transition = match cause {
            ReclaimCause::Released => TerminalTransition::Released {
                plugin: reason.plugin.clone(),
            },
            ReclaimCause::Deadline(forced) => TerminalTransition::ForceReclaimed {
                plugin: reason.plugin.clone(),
                diagnostic: forced.to_string(),
                timeout: forced.timeout,
            },
        };
        self.record(transition);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_engine::terminal_lease::ForcedReclaim;

    #[tokio::test]
    async fn fake_owner_records_a_yield_then_a_release() {
        let owner = FakeTerminalOwner::new();
        let transcript = owner.transcript();
        assert!(transcript.is_empty());

        let reason = LeaseReason::new("kiro", "device-code prompt");
        owner.yield_terminal(&reason).await.expect("yields");
        owner.reclaim_terminal(&reason, ReclaimCause::Released);

        assert_eq!(
            transcript.transitions(),
            vec![
                TerminalTransition::Acquired {
                    plugin: "kiro".to_owned(),
                    purpose: "device-code prompt".to_owned(),
                },
                TerminalTransition::Released {
                    plugin: "kiro".to_owned()
                },
            ]
        );
        assert!(transcript.acquired_by("kiro"));
        assert!(transcript.released_by("kiro"));
        assert_eq!(transcript.ownership_changes(), 2);
        assert_eq!(owner.yields_requested(), 1);
    }

    #[tokio::test]
    async fn fake_owner_refusal_is_not_an_ownership_change() {
        let owner = FakeTerminalOwner::refusing("no tty on this stdio session");
        let transcript = owner.transcript();

        let detail = owner
            .yield_terminal(&LeaseReason::new("kiro", "device-code prompt"))
            .await
            .expect_err("a refusing owner must refuse");

        assert_eq!(detail, "no tty on this stdio session");
        assert_eq!(transcript.len(), 1);
        assert_eq!(transcript.ownership_changes(), 0);
        assert!(!transcript.acquired_by("kiro"));
        assert_eq!(transcript.transitions()[0].plugin(), "kiro");
    }

    #[test]
    fn fake_owner_renders_the_forced_diagnostic_it_was_handed() {
        let owner = FakeTerminalOwner::new();
        let transcript = owner.transcript();
        let reason = LeaseReason::new("kiro", "device-code prompt");

        owner.reclaim_terminal(
            &reason,
            ReclaimCause::Deadline(ForcedReclaim {
                plugin: "kiro".to_owned(),
                purpose: "device-code prompt".to_owned(),
                timeout: Duration::from_millis(25),
            }),
        );

        let diagnostic = transcript
            .forced_diagnostic("kiro")
            .expect("a force-reclaim was recorded");
        assert!(diagnostic.contains("plugin `kiro`"), "{diagnostic}");
        assert!(diagnostic.contains("25 ms deadline"), "{diagnostic}");
        assert_eq!(transcript.forced_count(), 1);
        assert!(transcript.forced_diagnostic("other").is_none());
    }

    #[tokio::test]
    async fn fake_owner_wait_until_gives_up_on_something_that_never_happens() {
        let owner = FakeTerminalOwner::new();
        let transcript = owner.transcript();

        // A budget this small is safe precisely because the assertion is that the wait
        // *ends*, not that it ends quickly.
        let observed = transcript
            .wait_until(Duration::from_millis(5), |transitions| {
                !transitions.is_empty()
            })
            .await;
        assert!(!observed);
        assert!(
            transcript
                .wait_for_forced("kiro", Duration::ZERO)
                .await
                .is_none()
        );
        assert!(format!("{owner:?}").contains("yields_requested"));
    }
}

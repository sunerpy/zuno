//! Continuing a child session, and the board that decides which ones may be.
//!
//! # A continuation reuses a session; it does not rebuild one
//!
//! `task_id` names an existing child session and the next turn is appended **to it**.
//! Nothing is copied, replayed, or re-summarised: the context the child already holds
//! is the whole point, and reconstructing it would both cost the tokens the reuse was
//! meant to save and produce a transcript that differs from the one the child
//! actually saw. Upstream reaches the same conclusion in one line —
//! `nextSession = session ?? sessions.create(...)`
//! (`packages/opencode/src/tool/task.ts:167-172`) — and this module's seam is shaped
//! so the alternative is not expressible: [`ChildSessions`] can open a session, append
//! a turn, and count messages, and there is deliberately no operation that writes
//! history in bulk.
//!
//! # Two id spaces, because a lane outlives a dispatch
//!
//! A session id identifies the conversation; a job id identifies one dispatch into it.
//! They are independent, and one continuation keeps the session while taking a fresh
//! job id — so a completion notice can be matched to the dispatch it belongs to rather
//! than to whichever dispatch happens to be current. Upstream conflates them and pays
//! for it twice on one code path: `background.extend` reports `jobId: nextSession.id`
//! (`task.ts:256-263`) while `background.start` reports `jobId: info.id`
//! (`task.ts:288-295`), so the same field holds a session id or a service handle
//! depending on a branch the caller cannot see. [`JobBoard`] mints job ids from its own
//! sequence, and a test asserts none of them is ever a session id.
//!
//! # An `Active` job is not addressable
//!
//! This is the rule the board exists to enforce. Upstream's `extend` path
//! (`task.ts:256-271`) accepts a second `task` call into a lane that is already
//! running and reports "Background task updated" — the caller believes it amended the
//! running work, and whether the child ever observes the amendment depends on where
//! in its turn it was. `oh-my-opencode-slim` recognises the hazard and answers it with
//! prompt prose (`.omo/refs/omo-slim/src/agents/orchestrator.ts:226-231`: "A task in
//! the Active / Unreconciled section is still running and cannot receive another
//! `task` call, even with its `task_id`"), which is an instruction a model may
//! disregard. Here it is a refusal, and the refusal names the job, because the caller
//! needs to know *which* lane it must wait for.
//!
//! `Active` is **derived**, never stored twice. A lane is active when its own record
//! says it is still running, when it holds a terminal result the parent has not read
//! yet, **or** when [`RunState`] reports the child session busy. That last clause is
//! why there is no second notion of "running" in this crate: the authority is the
//! engine's run registry, and this module only asks it.
//!
//! One honest limit follows from that. The registry it defers to is process-local by
//! construction (`crates/oc-engine/src/status.rs:1-6`), so this board can refuse a
//! re-dispatch that would collide **inside this process** and cannot promise anything
//! about another one. The refusal is a guard against a caller's own bookkeeping drift,
//! not a distributed lock.
//!
//! # Prose is not a continuation
//!
//! The failure that motivates [`PROSE_IS_NOT_ENOUGH`]: a model writes "continuing the
//! previous explorer session", passes no `task_id`, and a fresh session starts
//! silently — the reuse it reported never happened, and the cost it thought it saved
//! was spent. slim states the rule in the orchestrator prompt
//! (`orchestrator.ts:245-247`); the board carries it instead, next to the aliases it
//! refers to, so the instruction and the data it names cannot drift apart.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// Prefix that keeps a job id distinguishable from a session id by inspection.
///
/// Matches the prefix the delegation tool already advertises for a background handle,
/// so a client that learned the shape from one layer recognises it in the other.
pub const JOB_ID_PREFIX: &str = "bg_";

/// The alias given to a lane whose agent name is empty.
///
/// An agent with no name cannot happen through the roster, but an alias is a handle a
/// model types, so it must never be the empty string.
pub const FALLBACK_ALIAS_PREFIX: &str = "job";

/// How many leading characters of an agent name become its alias prefix.
///
/// slim keeps a hand-written `AGENT_PREFIX` table
/// (`.omo/refs/omo-slim/src/utils/background-job-board.ts:119-127`) whose every entry
/// is the agent's own first three characters, so the table encodes nothing the name
/// does not already say. Taking the prefix from the name keeps a roster change from
/// needing an edit here — and a test asserts the roster's prefixes stay distinct,
/// which is the only property the table was protecting.
pub const ALIAS_PREFIX_LENGTH: usize = 3;

/// The board's heading, as it appears in the orchestrator's context.
pub const BOARD_HEADING: &str = "### Background Job Board";

/// The instruction that makes a claimed reuse mechanically checkable.
///
/// Stated as data rather than as prompt prose because a board that lists aliases and
/// an instruction that explains how to use them belong to the same artifact; kept
/// apart, a rename in one silently invalidates the other.
pub const PROSE_IS_NOT_ENOUGH: &str = concat!(
    "Claiming reuse in prose reuses nothing: a call whose `task_id` is absent or ",
    "empty starts a NEW session, whatever the surrounding text says. To continue a ",
    "job, pass its alias or its session id as `task_id`."
);

/// The addressability rule, stated where the states it refers to are rendered.
pub const ACTIVE_IS_NOT_ADDRESSABLE: &str = concat!(
    "A job under Active is still running or still owes a result, and cannot receive ",
    "another `task` call even with its `task_id` — a re-dispatch is refused, not ",
    "queued. Wait for it to report, then continue it once it appears under Reusable."
);

/// The one section whose jobs `task_id` may name.
pub const REUSABLE_RULE: &str =
    "Only a job under Reusable may be continued. Closed jobs are terminal: start fresh.";

/// A lane's addressability, derived from its record and from [`RunState`].
///
/// Five states rather than slim's `running | completed | error | cancelled |
/// reconciled` plus a separate `terminalUnreconciled` flag
/// (`background-job-board.ts:169-171`): the flag and the state are read together at
/// every decision point, so representing them separately only creates pairs that
/// cannot occur.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum JobState {
    /// Running now, or its session holds a live turn. Not addressable.
    Active,
    /// Finished, but the parent has not read the result yet. Not addressable, because
    /// a re-dispatch would overwrite an answer that is already waiting.
    Unreconciled,
    /// Finished, reconciled, and reusable — the one state `task_id` may name.
    Reconciled,
    /// Finished with an error. Its context ends at the failure, so it is not reusable.
    Failed,
    /// Cancelled before it finished. Partial context, so it is not reusable.
    Cancelled,
}

impl JobState {
    /// Every state, so a caller enumerating them cannot miss one.
    pub const ALL: [Self; 5] = [
        Self::Active,
        Self::Unreconciled,
        Self::Reconciled,
        Self::Failed,
        Self::Cancelled,
    ];

    /// Whether a `task_id` may name a job in this state.
    #[must_use]
    pub const fn addressable(self) -> bool {
        matches!(self, Self::Reconciled)
    }

    /// Which board section renders a job in this state.
    #[must_use]
    pub const fn section(self) -> Section {
        match self {
            Self::Active | Self::Unreconciled => Section::Active,
            Self::Reconciled => Section::Reusable,
            Self::Failed | Self::Cancelled => Section::Closed,
        }
    }
}

impl fmt::Display for JobState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Active => "active",
            Self::Unreconciled => "unreconciled",
            Self::Reconciled => "reconciled",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        })
    }
}

/// A board section, which is what actually communicates addressability.
///
/// A model reads a heading more reliably than it reads a per-row state word, so the
/// section is the primary signal and the state word is the detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Section {
    /// Not addressable: still working, or still owes a result.
    Active,
    /// Addressable by alias or session id.
    Reusable,
    /// Terminal and not reusable.
    Closed,
}

impl Section {
    /// Every section, in render order.
    pub const ALL: [Self; 3] = [Self::Active, Self::Reusable, Self::Closed];

    /// The rendered heading.
    #[must_use]
    pub const fn heading(self) -> &'static str {
        match self {
            Self::Active => "#### Active",
            Self::Reusable => "#### Reusable",
            Self::Closed => "#### Closed",
        }
    }
}

/// How a dispatch ended, as the layer that ran it observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Failed,
    Cancelled,
}

/// What the board recorded for one dispatch, before [`RunState`] is consulted.
///
/// Private to the module: the public answer is [`JobState`], and exposing the stored
/// half would invite a caller to read it instead of the derived state — which is how a
/// second, disagreeing notion of "running" gets introduced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recorded {
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// One lane on the board: a child session, its alias, and its current dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// The stable handle a caller passes as `task_id`. Minted once and never reissued.
    pub alias: String,
    /// The child session this lane continues.
    pub session_id: String,
    /// The agent the child runs as.
    pub agent: String,
    /// The most recent dispatch's job id.
    pub job_id: String,
    /// The derived state.
    pub state: JobState,
    /// What the lane was last asked to do, for a caller choosing between lanes.
    pub objective: String,
}

/// A lane's stored record.
#[derive(Debug, Clone)]
struct Lane {
    alias: String,
    session_id: String,
    agent: String,
    parent_session_id: String,
    job_id: String,
    recorded: Recorded,
    reconciled: bool,
    objective: String,
}

/// Why the child-session layer could not do what continuation asked of it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SessionStoreError(String);

impl SessionStoreError {
    /// Wrap a storage failure without leaking the storage layer's error type.
    #[must_use]
    pub fn new(detail: impl Into<String>) -> Self {
        Self(detail.into())
    }
}

/// The child-session effects continuation needs and this crate cannot perform.
///
/// A trait rather than a dependency on the session store, for the reason the manifest
/// records (`crates/oc-agent/Cargo.toml:12-21`): the delegation tool already depends
/// on this crate, so an edge from here to the layers above it would close a cycle.
///
/// The vocabulary is deliberately narrow. There is no `replace_history`, no
/// `copy_messages`, and no `replay` — a continuation must reuse a session in place,
/// and the cheapest way to guarantee that is to give the implementor no operation with
/// which to rebuild one.
pub trait ChildSessions: Send + Sync + 'static {
    /// Create a child session of `parent_session_id` running `agent`.
    ///
    /// Returns the new session id. Called only when no `task_id` resolved, which is
    /// what makes "a call without `task_id` starts a new session" observable.
    fn open(&self, parent_session_id: &str, agent: &str) -> Result<String, SessionStoreError>;

    /// Append one turn's prompt to `session_id` and report the session's new total.
    ///
    /// Append, not write: the count this returns is the evidence a continuation grew
    /// an existing conversation rather than restarting one.
    fn append_turn(&self, session_id: &str, prompt: &str) -> Result<usize, SessionStoreError>;

    /// How many messages `session_id` already holds, or [`None`] if it is gone.
    ///
    /// [`None`] is not zero. The board is process-local and the store is not, so a lane
    /// can outlive the session it names — compaction, an explicit delete, a store
    /// restored from an older copy. Appending to a session that no longer exists would
    /// create a conversation with one message and call it a continuation, so the answer
    /// has to distinguish "empty" from "absent".
    fn message_count(&self, session_id: &str) -> Result<Option<usize>, SessionStoreError>;
}

/// Whether a session holds a live turn right now.
///
/// The one intended implementation is the engine's run registry — `SessionStatus::Busy`
/// from `SessionRunRegistry::status` (`crates/oc-engine/src/status.rs:153-161`) — which
/// is also the thing that would reject the resumed turn with `SessionBusy`, so
/// deferring to it means the board refuses exactly what the engine would refuse.
///
/// That registry is not persisted (`status.rs:1-6`), so this trait answers for **this
/// process only**. A board built on it can catch a caller re-dispatching into a lane it
/// launched itself; it cannot see a turn another process is driving, and nothing here
/// should be read as claiming otherwise.
pub trait RunState: Send + Sync + 'static {
    /// `true` while `session_id` has an active turn in this process.
    fn is_running(&self, session_id: &str) -> bool;
}

/// No session is ever busy.
///
/// For a caller that has no run registry to consult — a batch tool, a test, a
/// board rendered outside a live process. Named for what it asserts rather than for
/// being a default, because "nothing is running" is a real claim and a caller that
/// picks it should know it is making one.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoLiveTurns;

impl RunState for NoLiveTurns {
    fn is_running(&self, _session_id: &str) -> bool {
        false
    }
}

/// One delegation, as the board receives it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Dispatch {
    /// The session delegating.
    pub parent_session_id: String,
    /// The agent the child runs as.
    pub agent: String,
    /// The caller's `task_id`: a lane's alias or its session id.
    ///
    /// [`None`] and `Some("")` mean the same thing — start a fresh lane — because a
    /// model that emits an empty string is not asking for a continuation, and treating
    /// the two differently would make the same intent succeed or fail on whitespace.
    pub task_id: Option<String>,
    /// The turn's text.
    pub prompt: String,
    /// What the lane is being asked to do, for the board.
    pub objective: String,
    /// Whether the caller asked not to wait.
    pub background: bool,
}

/// What a dispatch produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dispatched {
    /// This dispatch's handle. Fresh every time, even for a continuation.
    pub job_id: String,
    /// The child session, created or continued.
    pub session_id: String,
    /// The lane's stable handle.
    pub alias: String,
    /// Whether an existing session was continued.
    pub continued: bool,
    /// The child session's message count *before* this dispatch appended to it.
    ///
    /// Zero for a fresh lane. Reported alongside [`Self::message_count`] so "this
    /// continuation grew an existing conversation" is readable from one answer instead
    /// of inferred by comparing two.
    pub messages_before: usize,
    /// The child session's message count after this dispatch.
    pub message_count: usize,
}

/// Why a dispatch was refused.
///
/// Every variant names the lane it is about and states what to send instead. A
/// delegation refusal is read by a model, so a message that does not carry its own fix
/// buys nothing but a retry of the identical call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContinuationError {
    /// The named lane is running, or owes a result. The rule this module exists for.
    #[error(
        "`{alias}` (session `{session_id}`, agent `{agent}`, job `{job_id}`) is \
         {state} and cannot receive another `task` call, even with its `task_id`. A \
         running lane is not a queue: record the amendment in this conversation, wait \
         for `{alias}` to report, and continue it once the board lists it under \
         Reusable."
    )]
    ActiveLane {
        alias: String,
        session_id: String,
        agent: String,
        job_id: String,
        state: JobState,
    },

    /// `task_id` matched nothing on this board.
    #[error(
        "`task_id` `{task_id}` names no job on this board. Pass the alias or session \
         id of one that is addressable ({addressable}), or omit `task_id` to start a \
         fresh `{agent}` session — omitting it is the only way to start fresh, and \
         claiming reuse in prose continues nothing."
    )]
    UnknownTaskId {
        task_id: String,
        agent: String,
        addressable: String,
    },

    /// `task_id` named a lane belonging to a different parent session.
    #[error(
        "`task_id` `{task_id}` is a lane of session `{owner}`, not of `{parent}`, so \
         it is not yours to continue. Omit `task_id` to start a fresh `{agent}` \
         session."
    )]
    ForeignParent {
        task_id: String,
        owner: String,
        parent: String,
        agent: String,
    },

    /// `task_id` named a lane running a different agent.
    #[error(
        "`task_id` `{task_id}` is a `{recorded}` session; you asked for \
         `{requested}`. A session carries its agent's conduct and cannot be handed to \
         another — continue `{alias}` as `{recorded}`, or omit `task_id` to start a \
         fresh `{requested}` session."
    )]
    AgentMismatch {
        task_id: String,
        alias: String,
        recorded: String,
        requested: String,
    },

    /// `task_id` named a terminal lane whose context cannot be built on.
    #[error(
        "`{alias}` (session `{session_id}`, job `{job_id}`) is {state} and is not \
         reusable — its context stops where it stopped. Omit `task_id` to start a \
         fresh `{agent}` session."
    )]
    NotReusable {
        alias: String,
        session_id: String,
        agent: String,
        job_id: String,
        state: JobState,
    },

    /// The lane was addressable, but its session no longer exists in the store.
    #[error(
        "`{alias}` named session `{session_id}`, which no longer exists — its context \
         is gone, so there is nothing to continue. `{alias}` has been dropped from the \
         board; omit `task_id` to start a fresh `{agent}` session."
    )]
    VanishedSession {
        alias: String,
        session_id: String,
        agent: String,
    },

    /// The child-session layer failed.
    #[error("the child session layer failed: {0}")]
    Store(#[from] SessionStoreError),
}

/// Continuation and the background job board for one process.
///
/// Cloneable and shared: the orchestrator's turn loop renders the board, the
/// delegation tool dispatches through it, and the completion path settles jobs on it,
/// so all three must see one set of lanes.
#[derive(Clone)]
pub struct JobBoard {
    sessions: Arc<dyn ChildSessions>,
    runs: Arc<dyn RunState>,
    state: Arc<BoardState>,
}

struct BoardState {
    next_job: AtomicU64,
    lanes: Mutex<Lanes>,
}

#[derive(Default)]
struct Lanes {
    /// Dispatch order, so a render is deterministic and a lane's position is stable.
    order: Vec<Lane>,
    /// Per `(parent, alias prefix)` alias counter.
    counters: BTreeMap<(String, String), u64>,
}

impl JobBoard {
    /// A board over these two seams.
    #[must_use]
    pub fn new(sessions: Arc<dyn ChildSessions>, runs: Arc<dyn RunState>) -> Self {
        Self {
            sessions,
            runs,
            state: Arc::new(BoardState {
                next_job: AtomicU64::new(1),
                lanes: Mutex::new(Lanes::default()),
            }),
        }
    }

    /// Dispatch, continuing an existing lane when `task_id` resolves to an addressable
    /// one and starting a fresh lane when it is absent.
    ///
    /// The order of checks is the order in which a caller can act on the answer:
    /// resolve the handle, refuse an unusable lane, and only then touch the session
    /// store. A lane that cannot be continued must not have produced a session, or the
    /// refusal would leave an orphan behind that the next render would advertise.
    pub fn dispatch(&self, request: &Dispatch) -> Result<Dispatched, ContinuationError> {
        let requested = request
            .task_id
            .as_deref()
            .map(str::trim)
            .unwrap_or_default();
        let target = if requested.is_empty() {
            None
        } else {
            Some(self.resolve(request, requested)?)
        };

        match target {
            Some(lane) => self.continue_lane(request, lane),
            None => self.open_lane(request),
        }
    }

    /// Resolve `requested` to a continuable lane's `(session id, alias)`, or say why it
    /// is not one.
    fn resolve(
        &self,
        request: &Dispatch,
        requested: &str,
    ) -> Result<(String, String), ContinuationError> {
        let lanes = self.lock();
        let Some(lane) = lanes
            .order
            .iter()
            .find(|lane| lane.alias == requested || lane.session_id == requested)
        else {
            return Err(ContinuationError::UnknownTaskId {
                task_id: requested.to_owned(),
                agent: request.agent.clone(),
                addressable: render_addressable(&lanes, &request.parent_session_id, &*self.runs),
            });
        };

        if lane.parent_session_id != request.parent_session_id {
            return Err(ContinuationError::ForeignParent {
                task_id: requested.to_owned(),
                owner: lane.parent_session_id.clone(),
                parent: request.parent_session_id.clone(),
                agent: request.agent.clone(),
            });
        }
        if lane.agent != request.agent {
            return Err(ContinuationError::AgentMismatch {
                task_id: requested.to_owned(),
                alias: lane.alias.clone(),
                recorded: lane.agent.clone(),
                requested: request.agent.clone(),
            });
        }

        let state = derive(lane, &*self.runs);
        if state.addressable() {
            return Ok((lane.session_id.clone(), lane.alias.clone()));
        }
        Err(match state.section() {
            Section::Active => ContinuationError::ActiveLane {
                alias: lane.alias.clone(),
                session_id: lane.session_id.clone(),
                agent: lane.agent.clone(),
                job_id: lane.job_id.clone(),
                state,
            },
            Section::Reusable | Section::Closed => ContinuationError::NotReusable {
                alias: lane.alias.clone(),
                session_id: lane.session_id.clone(),
                agent: lane.agent.clone(),
                job_id: lane.job_id.clone(),
                state,
            },
        })
    }

    /// Append the turn to an existing session, keeping its alias and its history.
    ///
    /// The alias does not change and the session id does not change; only the job id
    /// does. That asymmetry is the point: the lane is the same lane, and this is a new
    /// dispatch into it.
    fn continue_lane(
        &self,
        request: &Dispatch,
        (session_id, lane_alias): (String, String),
    ) -> Result<Dispatched, ContinuationError> {
        // Read before appending, and outside the lock: the store is the authority on
        // whether the session still exists, and holding the board's mutex across a call
        // into it would let an implementation that reads the board deadlock.
        let Some(messages_before) = self.sessions.message_count(&session_id)? else {
            self.forget(&session_id);
            return Err(ContinuationError::VanishedSession {
                alias: lane_alias,
                session_id,
                agent: request.agent.clone(),
            });
        };
        let message_count = self.sessions.append_turn(&session_id, &request.prompt)?;
        let job_id = self.mint_job_id();

        let mut lanes = self.lock();
        let alias = match lanes
            .order
            .iter_mut()
            .find(|lane| lane.session_id == session_id)
        {
            Some(lane) => {
                lane.job_id = job_id.clone();
                lane.recorded = Recorded::Running;
                lane.reconciled = false;
                lane.objective = request.objective.clone();
                lane.alias.clone()
            }
            // Only reachable if the lane was removed between resolving and locking
            // again. Reporting it beats inventing an alias for a lane that is gone.
            None => {
                return Err(ContinuationError::UnknownTaskId {
                    task_id: session_id,
                    agent: request.agent.clone(),
                    addressable: render_addressable(
                        &lanes,
                        &request.parent_session_id,
                        &*self.runs,
                    ),
                });
            }
        };

        Ok(Dispatched {
            job_id,
            session_id,
            alias,
            continued: true,
            messages_before,
            message_count,
        })
    }

    /// Drop the lane naming `session_id`, so the next render stops advertising it.
    ///
    /// Its alias is not returned to the counter: a handle a model may already hold must
    /// never come to mean a different lane.
    fn forget(&self, session_id: &str) {
        let mut lanes = self.lock();
        lanes.order.retain(|lane| lane.session_id != session_id);
    }

    /// Open a fresh child session and record a new lane for it.
    fn open_lane(&self, request: &Dispatch) -> Result<Dispatched, ContinuationError> {
        let session_id = self
            .sessions
            .open(&request.parent_session_id, &request.agent)?;
        let message_count = self.sessions.append_turn(&session_id, &request.prompt)?;
        let job_id = self.mint_job_id();

        let mut lanes = self.lock();
        let alias = lanes.next_alias(&request.parent_session_id, &request.agent);
        lanes.order.push(Lane {
            alias: alias.clone(),
            session_id: session_id.clone(),
            agent: request.agent.clone(),
            parent_session_id: request.parent_session_id.clone(),
            job_id: job_id.clone(),
            recorded: Recorded::Running,
            reconciled: false,
            objective: request.objective.clone(),
        });

        Ok(Dispatched {
            job_id,
            session_id,
            alias,
            continued: false,
            messages_before: 0,
            message_count,
        })
    }

    /// Record how the dispatch `job_id` ended.
    ///
    /// Keyed on the job id rather than the session id because that is what a
    /// completion notice carries, and because a stale notice for a superseded dispatch
    /// must not settle the dispatch that replaced it. `false` means no lane is
    /// currently running that job.
    pub fn settle(&self, job_id: &str, outcome: Outcome) -> bool {
        let mut lanes = self.lock();
        let Some(lane) = lanes.order.iter_mut().find(|lane| lane.job_id == job_id) else {
            return false;
        };
        lane.recorded = match outcome {
            Outcome::Completed => Recorded::Completed,
            Outcome::Failed => Recorded::Failed,
            Outcome::Cancelled => Recorded::Cancelled,
        };
        lane.reconciled = false;
        true
    }

    /// Record that the parent has read `job_id`'s result.
    ///
    /// Separate from [`Self::settle`] because finishing and being read are different
    /// events, and only the second one makes a lane addressable: a lane whose answer is
    /// still waiting would lose it to a re-dispatch.
    pub fn reconcile(&self, job_id: &str) -> bool {
        let mut lanes = self.lock();
        let Some(lane) = lanes.order.iter_mut().find(|lane| lane.job_id == job_id) else {
            return false;
        };
        if lane.recorded == Recorded::Running {
            return false;
        }
        lane.reconciled = true;
        true
    }

    /// Every lane belonging to `parent_session_id`, in dispatch order.
    #[must_use]
    pub fn jobs(&self, parent_session_id: &str) -> Vec<Job> {
        let lanes = self.lock();
        lanes
            .order
            .iter()
            .filter(|lane| lane.parent_session_id == parent_session_id)
            .map(|lane| Job {
                alias: lane.alias.clone(),
                session_id: lane.session_id.clone(),
                agent: lane.agent.clone(),
                job_id: lane.job_id.clone(),
                state: derive(lane, &*self.runs),
                objective: lane.objective.clone(),
            })
            .collect()
    }

    /// The aliases a `task_id` may currently name, in dispatch order.
    #[must_use]
    pub fn addressable(&self, parent_session_id: &str) -> Vec<String> {
        self.jobs(parent_session_id)
            .into_iter()
            .filter(|job| job.state.addressable())
            .map(|job| job.alias)
            .collect()
    }

    /// Render the board for injection into `parent_session_id`'s context.
    ///
    /// [`None`] when the session has no lanes, so a first turn carries no board at all
    /// rather than an empty one — matching slim, which returns `undefined` in that case
    /// (`background-job-board.ts:661-663`).
    ///
    /// Nothing time-dependent is rendered. slim excludes wall-clock ages for the same
    /// reason (`background-job-board.ts:839-841`): the board is re-injected every turn,
    /// and a byte that changes when nothing happened both invalidates the provider's
    /// prompt-cache prefix and makes an unchanged lane look like it moved.
    #[must_use]
    pub fn render(&self, parent_session_id: &str) -> Option<String> {
        let jobs = self.jobs(parent_session_id);
        if jobs.is_empty() {
            return None;
        }

        let mut lines = vec![
            BOARD_HEADING.to_owned(),
            PROSE_IS_NOT_ENOUGH.to_owned(),
            ACTIVE_IS_NOT_ADDRESSABLE.to_owned(),
            REUSABLE_RULE.to_owned(),
        ];
        for section in Section::ALL {
            lines.push(String::new());
            lines.push(section.heading().to_owned());
            let mut any = false;
            for job in jobs.iter().filter(|job| job.state.section() == section) {
                any = true;
                lines.push(format!(
                    "- {} / {} / {} / {}",
                    job.alias, job.session_id, job.agent, job.state
                ));
                lines.push(format!("  Job: {}", job.job_id));
                lines.push(format!("  Objective: {}", one_line(&job.objective)));
            }
            if !any {
                lines.push("- none".to_owned());
            }
        }
        Some(lines.join("\n"))
    }

    /// A handle for this dispatch, from the board's own sequence.
    ///
    /// Not derived from the session id: a lane takes several dispatches over its life,
    /// and a derived id would give them all the same handle — which is the ambiguity
    /// upstream's `jobId: nextSession.id` (`task.ts:262`) creates.
    fn mint_job_id(&self) -> String {
        let sequence = self.state.next_job.fetch_add(1, Ordering::Relaxed);
        format!("{JOB_ID_PREFIX}{sequence:06}")
    }

    fn lock(&self) -> MutexGuard<'_, Lanes> {
        self.state
            .lanes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl fmt::Debug for JobBoard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let lanes = self.lock();
        formatter
            .debug_struct("JobBoard")
            .field("lanes", &lanes.order.len())
            .finish_non_exhaustive()
    }
}

impl Lanes {
    /// The next alias for `agent` under `parent`.
    ///
    /// Minted once per lane and never recomputed, which is what makes an alias a
    /// stable reference across turns: a scheme that renumbered on each render would
    /// break the handle a model read from the previous turn's board.
    fn next_alias(&mut self, parent: &str, agent: &str) -> String {
        let prefix = alias_prefix(agent);
        let counter = self
            .counters
            .entry((parent.to_owned(), prefix.clone()))
            .or_insert(0);
        *counter += 1;
        format!("{prefix}-{counter}")
    }
}

/// The alias prefix for `agent`.
#[must_use]
pub fn alias_prefix(agent: &str) -> String {
    let prefix: String = agent
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .take(ALIAS_PREFIX_LENGTH)
        .flat_map(char::to_lowercase)
        .collect();
    if prefix.is_empty() {
        FALLBACK_ALIAS_PREFIX.to_owned()
    } else {
        prefix
    }
}

/// The derived state of one lane.
///
/// [`RunState`] is consulted first and wins. A lane the board believes finished but
/// whose session still holds a live turn is `Active`, because the engine would reject
/// the resumed turn as busy — agreeing with it here turns a confusing downstream
/// failure into a refusal that names the lane.
///
/// `Unreconciled` applies to a *completed* lane only. slim routes every unread terminal
/// state through its Active section (`background-job-board.ts:662-664`), which reads as
/// "still working" for a lane that has already failed. The distinction that matters is
/// whether a re-dispatch would destroy an answer, and only a completed-but-unread lane
/// has one to destroy — so a failed or cancelled lane renders as what it is, and is
/// refused for a different reason.
fn derive(lane: &Lane, runs: &dyn RunState) -> JobState {
    if runs.is_running(&lane.session_id) {
        return JobState::Active;
    }
    match lane.recorded {
        Recorded::Running => JobState::Active,
        Recorded::Completed if lane.reconciled => JobState::Reconciled,
        Recorded::Completed => JobState::Unreconciled,
        Recorded::Failed => JobState::Failed,
        Recorded::Cancelled => JobState::Cancelled,
    }
}

/// The addressable aliases for a refusal message.
fn render_addressable(lanes: &Lanes, parent: &str, runs: &dyn RunState) -> String {
    let aliases: Vec<&str> = lanes
        .order
        .iter()
        .filter(|lane| lane.parent_session_id == parent && derive(lane, runs).addressable())
        .map(|lane| lane.alias.as_str())
        .collect();
    if aliases.is_empty() {
        "none yet".to_owned()
    } else {
        aliases.join(", ")
    }
}

/// Collapse an objective to one bounded line.
///
/// A board row that wraps or runs long stops being scannable, and a caller's objective
/// is free text that may be neither.
fn one_line(value: &str) -> String {
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= OBJECTIVE_LIMIT {
        return collapsed;
    }
    let kept: String = collapsed.chars().take(OBJECTIVE_LIMIT - 3).collect();
    format!("{kept}...")
}

/// How wide a rendered objective may be.
const OBJECTIVE_LIMIT: usize = 120;

/// One operation a [`ChildSessions`] implementation was asked to perform.
///
/// The op log is the artifact that makes "reuse in place" checkable: a continuation's
/// log is one [`Self::Append`] against an existing session, and a rebuild would show
/// up as an [`Self::Open`] or as a run of appends replaying old turns. Asserting the
/// *count* of messages alone cannot tell those apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionOp {
    Open { parent: String, agent: String },
    Append { session: String, prompt: String },
    Count { session: String },
}

/// Child sessions held in memory, recording every operation.
///
/// Public for the same reason the delegation tool's recording host is: the seam it
/// stands in for lives above this crate, so the layer wiring the real store needs
/// something to hold while doing it — and every assertion about continuation has to be
/// able to read back which operations a dispatch actually performed.
#[derive(Debug, Default)]
pub struct RecordingSessions {
    state: Mutex<RecordedSessions>,
}

#[derive(Debug, Default)]
struct RecordedSessions {
    next: u64,
    messages: BTreeMap<String, Vec<String>>,
    ops: Vec<SessionOp>,
    failure: Option<String>,
}

impl RecordingSessions {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fail every operation with `detail`, so the error path is a tested property.
    #[must_use]
    pub fn failing(detail: impl Into<String>) -> Self {
        Self {
            state: Mutex::new(RecordedSessions {
                failure: Some(detail.into()),
                ..RecordedSessions::default()
            }),
        }
    }

    /// Every operation this store received, in order.
    #[must_use]
    pub fn ops(&self) -> Vec<SessionOp> {
        self.lock().ops.clone()
    }

    /// The prompts recorded for `session_id`, in order.
    #[must_use]
    pub fn messages(&self, session_id: &str) -> Vec<String> {
        self.lock()
            .messages
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Delete `session_id`, as compaction or an explicit delete would.
    ///
    /// The board keeps its lane, which is the whole point: it reproduces the state where
    /// an in-process board still advertises a handle the store can no longer honour.
    pub fn delete(&self, session_id: &str) {
        self.lock().messages.remove(session_id);
    }

    fn lock(&self) -> MutexGuard<'_, RecordedSessions> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl ChildSessions for RecordingSessions {
    fn open(&self, parent_session_id: &str, agent: &str) -> Result<String, SessionStoreError> {
        let mut state = self.lock();
        if let Some(detail) = state.failure.clone() {
            return Err(SessionStoreError::new(detail));
        }
        state.ops.push(SessionOp::Open {
            parent: parent_session_id.to_owned(),
            agent: agent.to_owned(),
        });
        state.next += 1;
        let session_id = format!("ses_child_{:04}", state.next);
        state.messages.insert(session_id.clone(), Vec::new());
        Ok(session_id)
    }

    fn append_turn(&self, session_id: &str, prompt: &str) -> Result<usize, SessionStoreError> {
        let mut state = self.lock();
        if let Some(detail) = state.failure.clone() {
            return Err(SessionStoreError::new(detail));
        }
        state.ops.push(SessionOp::Append {
            session: session_id.to_owned(),
            prompt: prompt.to_owned(),
        });
        let messages = state.messages.entry(session_id.to_owned()).or_default();
        messages.push(prompt.to_owned());
        Ok(messages.len())
    }

    fn message_count(&self, session_id: &str) -> Result<Option<usize>, SessionStoreError> {
        let mut state = self.lock();
        if let Some(detail) = state.failure.clone() {
            return Err(SessionStoreError::new(detail));
        }
        state.ops.push(SessionOp::Count {
            session: session_id.to_owned(),
        });
        Ok(state
            .messages
            .get(session_id)
            .map(|messages| messages.len()))
    }
}

/// A [`RunState`] over an explicitly named set of busy sessions.
///
/// Stands in for the engine's registry so the derivation can be driven from a test
/// without an engine: `busy` here is exactly what `SessionStatus::Busy` means there.
#[derive(Debug, Default)]
pub struct StatedRunState {
    busy: Mutex<Vec<String>>,
}

impl StatedRunState {
    /// No session busy.
    #[must_use]
    pub fn idle() -> Self {
        Self::default()
    }

    /// Report `session_id` busy from now on.
    pub fn mark_busy(&self, session_id: impl Into<String>) {
        self.busy
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(session_id.into());
    }
}

impl RunState for StatedRunState {
    fn is_running(&self, session_id: &str) -> bool {
        self.busy
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|busy| busy == session_id)
    }
}

#[cfg(test)]
mod tests;

//! Pseudo-terminal management for interactive shell sessions with OS-level child containment.
//!
//! [`PtyService`] is the whole surface: open a terminal, resize it, write to it,
//! subscribe to its output, and remove it. It is a port of
//! `packages/core/src/pty.ts`, and it is deliberately a library with no HTTP in it
//! — the routes in todos 64-70 are a thin shell over these calls, exactly as the
//! oracle's two upgrade surfaces are over its `Pty.Service`.
//!
//! # Two bounds, both structural
//!
//! This crate is where an agent most easily leaks memory, so nothing here is
//! allowed to grow without a ceiling:
//!
//! | what | ceiling | where |
//! |---|---|---|
//! | one session's retained output | [`buffer::BUFFER_LIMIT`] (2 MiB) | [`buffer::ScrollbackBuffer`] |
//! | exited sessions kept observable | [`retention::EXITED_LIMIT`] (25) | [`retention::ExitRetention`] |
//! | queued output per attachment | [`session::DEFAULT_SUBSCRIBER_CAPACITY`] chunks | [`session::PtyOutput::Lagged`] |
//! | outstanding connect tickets | [`ticket::TICKET_CAPACITY`] | [`ticket::TicketStore`] |
//!
//! The first two are the oracle's own constants. The third and fourth are bounds
//! the oracle lacks: its per-subscriber `pending` array (`pty.ts:26`) and its
//! ticket cache admit unbounded growth from a client that connects and never
//! reads.
//!
//! # PTY operations stay synchronous
//!
//! Every [`PtyService`] operation is a plain `fn`. The underlying pty read and child
//! wait are blocking with no async form, so each session owns two OS threads and
//! publishes through non-blocking `try_send`. The separate
//! [`BackgroundExecutionService`] owns non-interactive Tokio child processes and
//! offers async wait/cancellation without changing the PTY contract.

pub mod background;
pub mod buffer;
pub mod retention;
pub mod session;
pub mod shells;
pub mod ticket;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use tokio::sync::broadcast;

pub use crate::background::{
    BackgroundExecutionError, BackgroundExecutionEvent, BackgroundExecutionId,
    BackgroundExecutionInfo, BackgroundExecutionInput, BackgroundExecutionOutput,
    BackgroundExecutionProjection, BackgroundExecutionService, BackgroundExecutionStatus,
    BackgroundWaitOutcome,
};
pub use crate::buffer::{BUFFER_LIMIT, Replay, ReplayCursor, ScrollbackBuffer};
pub use crate::retention::EXITED_LIMIT;
pub use crate::session::{
    AttachOptions, Attachment, CreateInput, DEFAULT_SUBSCRIBER_CAPACITY, DRAIN_GRACE, PtyId,
    PtyInfo, PtyOutput, PtyStatus, RetainedOutput, SessionOptions, TerminalSize, UpdateInput,
};
pub use crate::shells::ShellItem;
pub use crate::ticket::{ConnectToken, TicketScope, TicketStore};

use crate::retention::ExitRetention;
use crate::session::{ExitObserver, SessionHandle};

/// Retained cause of a failure whose origin is a third-party error type.
pub type BoxSource = Box<dyn std::error::Error + Send + Sync>;

/// Lifecycle events broadcast to clients per `packages/schema/src/pty.ts:34-38`.
///
/// The `type` strings are the oracle's. The envelope a server wraps these in
/// (topic, sequence number, workspace) is the server's concern, not this crate's.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "type", content = "properties")]
pub enum PtyEvent {
    /// A session was created.
    #[serde(rename = "pty.created")]
    Created {
        /// The new session.
        info: PtyInfo,
    },
    /// A session's title or size changed.
    #[serde(rename = "pty.updated")]
    Updated {
        /// The session after the change.
        info: PtyInfo,
    },
    /// A session's child exited. The session stays observable until deleted.
    #[serde(rename = "pty.exited")]
    Exited {
        /// The session that exited.
        id: PtyId,
        /// Its exit code, absent when the platform could not report one.
        #[serde(rename = "exitCode", skip_serializing_if = "Option::is_none")]
        exit_code: Option<u32>,
    },
    /// A session was removed, either explicitly or by the retention cap.
    #[serde(rename = "pty.deleted")]
    Deleted {
        /// The session that is gone.
        id: PtyId,
    },
}

/// Every way a PTY operation can fail.
///
/// [`Self::NotFound`] and [`Self::Exited`] are the oracle's two errors
/// (`pty.ts:74-80`). The I/O variants are additions: the oracle wraps its write,
/// resize and kill calls in bare `try {} catch {}`, so a client whose keystrokes
/// are silently going nowhere has no way to learn that.
#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    /// No session with this identifier, or it was evicted by the retention cap.
    #[error("pty session `{id}` does not exist")]
    NotFound {
        /// The identifier that was looked up.
        id: PtyId,
    },
    /// The session exists but its child has exited, so it cannot be attached to.
    #[error("pty session `{id}` has exited")]
    Exited {
        /// The exited session.
        id: PtyId,
    },
    /// The platform refused a new pty, typically at the per-user limit.
    #[error("could not open a pty for `{command}`")]
    Open {
        /// The command the pty was being opened for.
        command: String,
        /// The platform's reason.
        #[source]
        source: BoxSource,
    },
    /// The pty opened but the command could not be executed in it.
    #[error("could not spawn `{command}` in a pty")]
    Spawn {
        /// The command that could not be executed.
        command: String,
        /// The platform's reason.
        #[source]
        source: BoxSource,
    },
    /// Terminal input could not be delivered to the child.
    #[error("could not write to pty session `{id}`")]
    Write {
        /// The session that was being written to.
        id: PtyId,
        /// The platform's reason.
        #[source]
        source: std::io::Error,
    },
    /// The kernel's window size could not be read or changed.
    #[error("could not resize pty session `{id}`")]
    Resize {
        /// The session that was being resized.
        id: PtyId,
        /// The platform's reason.
        #[source]
        source: BoxSource,
    },
}

/// Events buffered for a lagging subscriber before it is told it lagged.
///
/// Lifecycle events are small and rare — four per session at most — so 1,024 slots
/// absorb a burst of the retention cap evicting 25 sessions at once many times
/// over. A subscriber slower than that gets `RecvError::Lagged` from
/// [`broadcast::Receiver`] and should re-list.
pub const DEFAULT_EVENT_CAPACITY: usize = 1_024;

#[derive(Debug)]
struct Registry {
    sessions: HashMap<PtyId, Arc<SessionHandle>>,
    retention: ExitRetention,
}

/// Connects a session's exit to the service that owns it.
///
/// Weak, so a session outliving its service cannot keep the service alive; a dropped
/// service has already killed and cleared everything, so an exit arriving afterwards
/// has nothing left to report.
struct RegistryExitObserver {
    inner: Weak<ServiceInner>,
}

impl ExitObserver for RegistryExitObserver {
    fn announce(&self, id: &PtyId, exit_code: Option<u32>) {
        if let Some(inner) = self.inner.upgrade() {
            PtyService { inner }.publish(PtyEvent::Exited {
                id: id.clone(),
                exit_code,
            });
        }
    }

    fn record(&self, id: &PtyId) {
        if let Some(inner) = self.inner.upgrade() {
            PtyService { inner }.retain_exit(id);
        }
    }
}

#[derive(Debug)]
struct ServiceInner {
    registry: Mutex<Registry>,
    options: SessionOptions,
    events: broadcast::Sender<PtyEvent>,
    tickets: TicketStore,
}

impl Drop for ServiceInner {
    /// Kills every child and clears both maps, as the service finalizer does at
    /// `packages/core/src/pty.ts:130-133`.
    ///
    /// Without this, dropping the service would leave its shells running as
    /// orphans holding the process's ptys open.
    fn drop(&mut self) {
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for session in registry.sessions.values() {
            session.shutdown();
        }
        registry.sessions.clear();
        registry.retention.clear();
    }
}

/// How a [`PtyService`] is configured, all of it at construction time.
///
/// The two limit overrides exist so a test can reach a bounded path with kilobytes
/// and a handful of sessions instead of megabytes and twenty-six; production uses
/// [`PtyServiceConfig::new`], which carries the oracle's constants.
#[derive(Debug, Clone)]
pub struct PtyServiceConfig {
    /// Directory a create request without a `cwd` starts in.
    pub directory: PathBuf,
    /// Per-session scrollback ceiling in bytes.
    pub buffer_limit: usize,
    /// How many exited sessions stay observable.
    pub exited_limit: usize,
    /// The `shell` config value, used when a create request names no command.
    pub configured_shell: Option<String>,
}

impl PtyServiceConfig {
    /// The production configuration for `directory`.
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            buffer_limit: BUFFER_LIMIT,
            exited_limit: EXITED_LIMIT,
            configured_shell: None,
        }
    }

    /// Sets the `shell` config value. An empty string is treated as unset.
    #[must_use]
    pub fn with_configured_shell(mut self, shell: Option<String>) -> Self {
        self.configured_shell = shell.filter(|value| !value.is_empty());
        self
    }

    /// Sets the per-session scrollback ceiling.
    #[must_use]
    pub const fn with_buffer_limit(mut self, limit: usize) -> Self {
        self.buffer_limit = limit;
        self
    }

    /// Sets how many exited sessions stay observable.
    #[must_use]
    pub const fn with_exited_limit(mut self, limit: usize) -> Self {
        self.exited_limit = limit;
        self
    }
}

/// PTY sessions for one instance, with both retention bounds enforced.
///
/// Cheap to clone: every clone shares one set of sessions, so a server can put it
/// in router state and a CLI can hold the same one.
#[derive(Debug, Clone)]
pub struct PtyService {
    inner: Arc<ServiceInner>,
}

impl PtyService {
    /// Creates a service whose sessions default to `directory`.
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self::with_config(PtyServiceConfig::new(directory))
    }

    /// Creates a service from an explicit configuration.
    #[must_use]
    pub fn with_config(config: PtyServiceConfig) -> Self {
        Self {
            inner: Arc::new(ServiceInner {
                registry: Mutex::new(Registry {
                    sessions: HashMap::new(),
                    retention: ExitRetention::with_limit(config.exited_limit),
                }),
                options: SessionOptions {
                    buffer_limit: config.buffer_limit,
                    default_cwd: config.directory,
                    configured_shell: config.configured_shell,
                },
                events: broadcast::channel(DEFAULT_EVENT_CAPACITY).0,
                tickets: TicketStore::new(),
            }),
        }
    }

    /// Every shell on this machine, for `GET /pty/shells`.
    #[must_use]
    pub fn shells(&self) -> Vec<ShellItem> {
        shells::list()
    }

    /// Lifecycle events from now on. Late subscribers do not see past events.
    ///
    /// Every event is published before the state change it reports becomes readable
    /// through this service, so a consumer never has to poll for one it has already
    /// seen the effect of:
    ///
    /// | event | in the channel by the time… |
    /// |---|---|
    /// | `Created` | [`Self::create`] returns |
    /// | `Updated` | [`Self::update`] returns |
    /// | `Exited` | [`Self::get`] can report [`PtyStatus::Exited`] |
    /// | `Deleted` | [`Self::remove`] returns |
    ///
    /// The `Exited` row is the one that needed enforcing rather than falling out of
    /// the code — see [`crate::session`]'s `mark_exited`.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<PtyEvent> {
        self.inner.events.subscribe()
    }

    /// The connect-ticket store shared by every upgrade surface.
    #[must_use]
    pub fn tickets(&self) -> &TicketStore {
        &self.inner.tickets
    }

    /// Every session, running and retained-exited, in creation order.
    ///
    /// The oracle relies on JavaScript `Map` insertion order (`pty.ts:158`). Sorting
    /// by identifier reproduces it without depending on a hash map's iteration
    /// order, because [`PtyId::mint`] is monotonic.
    #[must_use]
    pub fn list(&self) -> Vec<PtyInfo> {
        let registry = self.lock();
        let mut sessions: Vec<PtyInfo> = registry
            .sessions
            .values()
            .map(|session| session.info())
            .collect();
        sessions.sort_by(|left, right| left.id.cmp(&right.id));
        sessions
    }

    /// Opens a pty and spawns a command in it.
    ///
    /// # Errors
    ///
    /// [`PtyError::Open`] or [`PtyError::Spawn`]; see their docs.
    pub fn create(&self, input: CreateInput) -> Result<PtyInfo, PtyError> {
        let session = SessionHandle::spawn(input, &self.inner.options, self.exit_observer())?;
        let info = session.info();
        self.lock().sessions.insert(session.id().clone(), session);
        self.publish(PtyEvent::Created { info: info.clone() });
        Ok(info)
    }

    /// One session's current state.
    ///
    /// # Errors
    ///
    /// [`PtyError::NotFound`] when the identifier is unknown or was evicted.
    pub fn get(&self, id: &PtyId) -> Result<PtyInfo, PtyError> {
        self.session(id).map(|session| session.info())
    }

    /// Applies a title change, a resize, or both.
    ///
    /// A resize of an exited session is accepted and ignored rather than rejected,
    /// matching the `status === "running"` guard at `pty.ts:194` — a client
    /// reporting its window size should not have to know whether the shell is still
    /// alive.
    ///
    /// # Errors
    ///
    /// [`PtyError::NotFound`], or [`PtyError::Resize`] when the platform rejects
    /// the new size.
    pub fn update(&self, id: &PtyId, input: UpdateInput) -> Result<PtyInfo, PtyError> {
        let session = self.session(id)?;
        if let Some(title) = input.title.filter(|title| !title.is_empty()) {
            session.set_title(title);
        }
        if let Some(size) = input.size {
            session.resize(size)?;
        }
        let info = session.info();
        self.publish(PtyEvent::Updated { info: info.clone() });
        Ok(info)
    }

    /// Terminates a session and forgets it, including its retained output.
    ///
    /// # Errors
    ///
    /// [`PtyError::NotFound`] when the identifier is unknown.
    pub fn remove(&self, id: &PtyId) -> Result<(), PtyError> {
        if self.take(id).is_none() {
            return Err(PtyError::NotFound { id: id.clone() });
        }
        Ok(())
    }

    /// Sends terminal input. Silently accepted once the child exited.
    ///
    /// # Errors
    ///
    /// [`PtyError::NotFound`], or [`PtyError::Write`] when the pty rejects it.
    pub fn write(&self, id: &PtyId, data: &[u8]) -> Result<(), PtyError> {
        self.session(id)?.write(data)
    }

    /// Subscribes to a running session, with a replay of retained output.
    ///
    /// The returned [`Attachment`] detaches on drop.
    ///
    /// # Errors
    ///
    /// [`PtyError::NotFound`], or [`PtyError::Exited`] when the child already
    /// exited — its output is still readable with [`Self::retained_output`].
    pub fn attach(&self, id: &PtyId, options: AttachOptions) -> Result<Attachment, PtyError> {
        self.session(id)?.attach(options)
    }

    /// A snapshot of one session's retained output, running or exited.
    ///
    /// # Errors
    ///
    /// [`PtyError::NotFound`] when the identifier is unknown.
    pub fn retained_output(&self, id: &PtyId) -> Result<RetainedOutput, PtyError> {
        self.session(id).map(|session| session.retained_output())
    }

    /// The size the kernel reports for a session.
    ///
    /// # Errors
    ///
    /// [`PtyError::NotFound`], or [`PtyError::Resize`] when the platform cannot
    /// report it.
    pub fn size(&self, id: &PtyId) -> Result<TerminalSize, PtyError> {
        self.session(id)?.size()
    }

    /// Bytes reserved by a session's scrollback ring, for asserting the bound.
    ///
    /// # Errors
    ///
    /// [`PtyError::NotFound`] when the identifier is unknown.
    pub fn reserved_bytes(&self, id: &PtyId) -> Result<usize, PtyError> {
        self.session(id).map(|session| session.reserved_bytes())
    }

    /// Live attachments to a session, for asserting detachment actually happened.
    ///
    /// # Errors
    ///
    /// [`PtyError::NotFound`] when the identifier is unknown.
    pub fn subscriber_count(&self, id: &PtyId) -> Result<usize, PtyError> {
        self.session(id).map(|session| session.subscriber_count())
    }

    /// Whether a session is still known.
    #[must_use]
    pub fn contains(&self, id: &PtyId) -> bool {
        self.lock().sessions.contains_key(id)
    }

    /// The retained exited sessions, oldest exit first.
    ///
    /// The order is the eviction order, so the first element is the next to go.
    #[must_use]
    pub fn retained_exited(&self) -> Vec<PtyId> {
        self.lock().retention.retained().cloned().collect()
    }

    /// The cap on retained exited sessions.
    #[must_use]
    pub fn exited_limit(&self) -> usize {
        self.lock().retention.limit()
    }

    /// The default directory new sessions start in.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.inner.options.default_cwd
    }

    fn session(&self, id: &PtyId) -> Result<Arc<SessionHandle>, PtyError> {
        self.lock()
            .sessions
            .get(id)
            .map(Arc::clone)
            .ok_or_else(|| PtyError::NotFound { id: id.clone() })
    }

    /// Removes a session, tears it down, and announces it.
    ///
    /// The teardown happens with the registry lock released: `shutdown` takes the
    /// session's own lock to end its subscriptions, and holding the registry across
    /// that would let a reader thread appending output block every other session's
    /// lookups.
    fn take(&self, id: &PtyId) -> Option<Arc<SessionHandle>> {
        let session = {
            let mut registry = self.lock();
            registry.retention.forget(id);
            registry.sessions.remove(id)
        }?;
        session.shutdown();
        self.inner.tickets.revoke_session(id);
        self.publish(PtyEvent::Deleted { id: id.clone() });
        Some(session)
    }

    /// Retains an exited session, then evicts whatever the cap displaced.
    ///
    /// Runs on the exiting session's waiter thread, after its state lock is released
    /// — it takes the registry lock, and [`Self::list`] already holds
    /// registry-then-session, so acquiring it under a session lock would invert the
    /// order. That is why the exit's *announcement* is a separate phase.
    ///
    /// A session removed between the announcement and this call retains nothing: its
    /// identifier is already gone, so a retention entry would name a session no
    /// lookup could reach. The event went out either way, which is the difference
    /// from the bug this shape replaced.
    fn retain_exit(&self, id: &PtyId) {
        let evicted = {
            let mut registry = self.lock();
            if !registry.sessions.contains_key(id) {
                return;
            }
            registry.retention.record_exit(id.clone())
        };
        for stale in evicted {
            tracing::debug!(%stale, "evicting an exited pty session at the retention cap");
            self.take(&stale);
        }
    }

    fn exit_observer(&self) -> Arc<dyn ExitObserver> {
        Arc::new(RegistryExitObserver {
            inner: Arc::downgrade(&self.inner),
        })
    }

    fn publish(&self, event: PtyEvent) {
        // No receivers is the normal case for a CLI; it is not a failure.
        let _delivered = self.inner.events.send(event);
    }

    fn lock(&self) -> MutexGuard<'_, Registry> {
        self.inner
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_upstream_constants_are_carried_verbatim() {
        assert_eq!(BUFFER_LIMIT, 1024 * 1024 * 2, "packages/core/src/pty.ts:14");
        assert_eq!(EXITED_LIMIT, 25, "packages/core/src/pty.ts:17");
    }

    #[test]
    fn events_serialize_with_the_oracle_type_strings() {
        let exited = PtyEvent::Exited {
            id: PtyId::from_raw("pty_one"),
            exit_code: Some(7),
        };
        let json = serde_json::to_value(&exited).expect("PtyEvent serializes");
        assert_eq!(json["type"], "pty.exited");
        assert_eq!(json["properties"]["id"], "pty_one");
        assert_eq!(json["properties"]["exitCode"], 7);

        let deleted = PtyEvent::Deleted {
            id: PtyId::from_raw("pty_one"),
        };
        let json = serde_json::to_value(&deleted).expect("PtyEvent serializes");
        assert_eq!(json["type"], "pty.deleted");
    }

    #[test]
    fn a_new_service_is_empty_and_reports_its_bounds() {
        let service = PtyService::new("/tmp");
        assert!(service.list().is_empty());
        assert!(service.retained_exited().is_empty());
        assert_eq!(service.exited_limit(), EXITED_LIMIT);
        assert_eq!(service.directory(), Path::new("/tmp"));
    }

    #[test]
    fn an_unknown_identifier_is_not_found_on_every_lookup() {
        let service = PtyService::new("/tmp");
        let missing = PtyId::from_raw("pty_missing");
        assert!(matches!(
            service.get(&missing),
            Err(PtyError::NotFound { .. })
        ));
        assert!(matches!(
            service.remove(&missing),
            Err(PtyError::NotFound { .. })
        ));
        assert!(matches!(
            service.write(&missing, b"x"),
            Err(PtyError::NotFound { .. })
        ));
        assert!(matches!(
            service.retained_output(&missing),
            Err(PtyError::NotFound { .. })
        ));
        assert!(matches!(
            service.attach(&missing, AttachOptions::default()),
            Err(PtyError::NotFound { .. })
        ));
        assert!(!service.contains(&missing));
    }

    #[test]
    fn the_config_overrides_reach_the_service() {
        let service = PtyService::with_config(
            PtyServiceConfig::new("/tmp")
                .with_buffer_limit(4_096)
                .with_exited_limit(3)
                .with_configured_shell(Some("/bin/sh".to_owned())),
        );
        assert_eq!(service.exited_limit(), 3);
        assert_eq!(service.inner.options.buffer_limit, 4_096);
        assert_eq!(
            service.inner.options.configured_shell.as_deref(),
            Some("/bin/sh")
        );
    }

    #[test]
    fn an_empty_configured_shell_is_treated_as_unset() {
        let config = PtyServiceConfig::new("/tmp").with_configured_shell(Some(String::new()));
        assert!(config.configured_shell.is_none());
    }
}

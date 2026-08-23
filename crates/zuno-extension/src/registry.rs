use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use serde::Serialize;

use crate::{Package, StaticPackage, resolve::resolve_active_packages};

/// The workspace identity that owns process-local definitions.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Scope(PathBuf);

impl Scope {
    #[must_use]
    pub fn new(root: &Path) -> Self {
        Self(root.canonicalize().unwrap_or_else(|_| root.to_path_buf()))
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.0
    }
}

/// Process-local lifecycle state visible to tools and clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DynamicState {
    Defined,
    PendingRun,
    Running,
    PendingStop,
    Stopped,
    PendingUndefine,
    Uncertain,
}

/// One process-local definition as exposed by inspection.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PackageStatus {
    pub id: String,
    pub description: String,
    pub state: DynamicState,
    pub agents: Vec<String>,
    pub workflows: Vec<String>,
    pub skills: Vec<String>,
    pub tools: Vec<String>,
    pub runtime: Option<String>,
}

/// Token identifying one prepared composition mutation.
///
/// The registry commits only the exact pending token for the same workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionTransaction {
    scope: Scope,
    revision: u64,
}

impl ExtensionTransaction {
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub fn scope(&self) -> &Scope {
        &self.scope
    }
}

/// Whether a lifecycle request changed the desired active composition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageOutcome {
    Unchanged { revision: u64 },
    Pending(ExtensionTransaction),
}

/// Proof that one live consumer owns the committed composition revision.
#[derive(Debug)]
pub struct CompositionLease {
    registry: Arc<ExtensionRegistry>,
    scope: Scope,
    revision: u64,
    released: bool,
}

impl CompositionLease {
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Release the consumer while permanently blocking further transitions.
    pub fn mark_uncertain(mut self, message: impl Into<String>) {
        self.registry
            .release_lease(&self.scope, self.revision, Some(message.into()));
        self.released = true;
    }
}

impl Drop for CompositionLease {
    fn drop(&mut self) {
        if !self.released {
            self.registry
                .release_lease(&self.scope, self.revision, None);
            self.released = true;
        }
    }
}

/// Exclusive reservation for starting one desired composition.
#[derive(Debug)]
pub struct PreparedTransition {
    registry: Arc<ExtensionRegistry>,
    transaction: ExtensionTransaction,
    finished: bool,
}

impl PreparedTransition {
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.transaction.revision
    }

    #[must_use]
    pub fn scope(&self) -> &Scope {
        &self.transaction.scope
    }

    /// Publish the desired mutation and return its first active-consumer lease.
    pub fn commit(mut self) -> Result<CompositionLease, RegistryError> {
        let lease = self.registry.commit_reserved(&self.transaction)?;
        self.finished = true;
        Ok(lease)
    }

    /// Candidate preparation failed before any uncertain side effect remained.
    pub fn abort(mut self) -> Result<(), RegistryError> {
        self.registry.abort_reserved(&self.transaction)?;
        self.finished = true;
        Ok(())
    }

    /// Candidate cleanup could not prove quiescence.
    pub fn mark_uncertain(mut self, message: impl Into<String>) {
        self.registry
            .uncertain_reserved(&self.transaction, message.into());
        self.finished = true;
    }
}

impl Drop for PreparedTransition {
    fn drop(&mut self) {
        if !self.finished {
            self.registry.uncertain_reserved(
                &self.transaction,
                "a prepared extension transition was dropped without an explicit commit or abort"
                    .to_owned(),
            );
            self.finished = true;
        }
    }
}

impl StageOutcome {
    #[must_use]
    pub const fn revision(&self) -> u64 {
        match self {
            Self::Unchanged { revision } => *revision,
            Self::Pending(transaction) => transaction.revision,
        }
    }

    #[must_use]
    pub const fn is_pending(&self) -> bool {
        matches!(self, Self::Pending(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommittedState {
    Defined,
    Running,
    Stopped,
}

#[derive(Debug, Clone)]
struct DynamicPackage {
    package: Package,
    state: CommittedState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingKind {
    Run,
    Stop,
    Undefine,
}

#[derive(Debug, Clone)]
struct PendingMutation {
    id: String,
    kind: PendingKind,
    revision: u64,
}

#[derive(Debug, Default)]
struct ScopeRegistry {
    packages: Vec<DynamicPackage>,
    active_revision: u64,
    next_revision: u64,
    pending: Option<PendingMutation>,
    transition_reserved: bool,
    active_consumers: u64,
    uncertain: Option<String>,
}

/// In-memory definitions owned by one Zuno process.
///
/// Active and desired compositions are separate. Lifecycle tools only stage a
/// desired mutation. A consumer must build that desired composition, quiesce the
/// previous owner, and then acknowledge the exact transaction with [`Self::commit`].
#[derive(Debug, Default)]
pub struct ExtensionRegistry {
    scopes: RwLock<BTreeMap<Scope, ScopeRegistry>>,
}

impl ExtensionRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Revision currently acknowledged by the composition consumer.
    #[must_use]
    pub fn active_revision(&self, scope: &Scope) -> u64 {
        self.read()
            .get(scope)
            .map_or(0, |registry| registry.active_revision)
    }

    /// Revision the next prepared consumer must assemble.
    #[must_use]
    pub fn desired_revision(&self, scope: &Scope) -> u64 {
        self.read().get(scope).map_or(0, |registry| {
            registry
                .pending
                .as_ref()
                .map_or(registry.active_revision, |pending| pending.revision)
        })
    }

    #[must_use]
    pub fn pending_transaction(&self, scope: &Scope) -> Option<ExtensionTransaction> {
        self.read()
            .get(scope)
            .and_then(|registry| registry.pending.as_ref())
            .map(|pending| ExtensionTransaction {
                scope: scope.clone(),
                revision: pending.revision,
            })
    }

    /// Why this scope refuses further lifecycle operations, when cleanup was uncertain.
    #[must_use]
    pub fn uncertainty(&self, scope: &Scope) -> Option<String> {
        self.read()
            .get(scope)
            .and_then(|registry| registry.uncertain.clone())
    }

    /// Register a live consumer of the committed revision.
    pub fn acquire_active(
        self: &Arc<Self>,
        scope: &Scope,
        revision: u64,
    ) -> Result<CompositionLease, RegistryError> {
        let mut scopes = self.write();
        let scoped = scopes.entry(scope.clone()).or_default();
        reject_uncertain(scoped)?;
        if scoped.transition_reserved {
            return Err(RegistryError::TransitionReserved);
        }
        if scoped.active_revision != revision {
            return Err(RegistryError::RevisionMismatch {
                expected: scoped.active_revision,
                actual: revision,
            });
        }
        scoped.active_consumers = scoped
            .active_consumers
            .checked_add(1)
            .ok_or(RegistryError::ConsumerCountExhausted)?;
        Ok(CompositionLease {
            registry: Arc::clone(self),
            scope: scope.clone(),
            revision,
            released: false,
        })
    }

    /// Reserve a pending mutation only after every previous consumer has stopped.
    pub fn begin_transition(
        self: &Arc<Self>,
        transaction: &ExtensionTransaction,
    ) -> Result<PreparedTransition, RegistryError> {
        let mut scopes = self.write();
        let scoped =
            scopes
                .get_mut(&transaction.scope)
                .ok_or(RegistryError::TransactionMismatch {
                    revision: transaction.revision,
                })?;
        reject_uncertain(scoped)?;
        let matches = scoped
            .pending
            .as_ref()
            .is_some_and(|pending| pending.revision == transaction.revision);
        if !matches {
            return Err(RegistryError::TransactionMismatch {
                revision: transaction.revision,
            });
        }
        if scoped.transition_reserved {
            return Err(RegistryError::TransitionReserved);
        }
        if scoped.active_consumers != 0 {
            return Err(RegistryError::ActiveConsumers {
                count: scoped.active_consumers,
            });
        }
        scoped.transition_reserved = true;
        Ok(PreparedTransition {
            registry: Arc::clone(self),
            transaction: transaction.clone(),
            finished: false,
        })
    }

    pub fn define(&self, scope: &Scope, package: Package) -> Result<(), RegistryError> {
        package.validate().map_err(RegistryError::Manifest)?;
        if package.runtime.is_some() {
            return Err(RegistryError::DynamicExecutable(package.id));
        }
        let mut scopes = self.write();
        let scoped = scopes.entry(scope.clone()).or_default();
        reject_uncertain(scoped)?;
        if scoped
            .packages
            .iter()
            .any(|entry| entry.package.id == package.id)
        {
            return Err(RegistryError::AlreadyDefined(package.id));
        }
        scoped.packages.push(DynamicPackage {
            package,
            state: CommittedState::Defined,
        });
        Ok(())
    }

    /// Prepare activation against static packages and the committed composition.
    pub fn stage_run(
        &self,
        scope: &Scope,
        id: &str,
        static_packages: &[StaticPackage],
    ) -> Result<StageOutcome, RegistryError> {
        let mut scopes = self.write();
        let scoped = scopes
            .get_mut(scope)
            .ok_or_else(|| RegistryError::NotFound(id.to_owned()))?;
        reject_uncertain(scoped)?;
        reject_pending(scoped)?;
        let target = package_index(scoped, id)?;
        if scoped.packages[target].state == CommittedState::Running {
            return Ok(StageOutcome::Unchanged {
                revision: scoped.active_revision,
            });
        }
        let candidate = scoped
            .packages
            .iter()
            .enumerate()
            .filter(|(index, entry)| *index == target || entry.state == CommittedState::Running)
            .map(|(_, entry)| entry.package.clone())
            .collect::<Vec<_>>();
        resolve_active_packages(static_packages, candidate)
            .map_err(|error| RegistryError::Activation(Box::new(error)))?;
        stage(scope, scoped, id, PendingKind::Run)
    }

    /// Prepare deactivation while leaving the committed composition untouched.
    pub fn stage_stop(&self, scope: &Scope, id: &str) -> Result<StageOutcome, RegistryError> {
        let mut scopes = self.write();
        let scoped = scopes
            .get_mut(scope)
            .ok_or_else(|| RegistryError::NotFound(id.to_owned()))?;
        reject_uncertain(scoped)?;
        reject_pending(scoped)?;
        let target = package_index(scoped, id)?;
        match scoped.packages[target].state {
            CommittedState::Running => stage(scope, scoped, id, PendingKind::Stop),
            CommittedState::Defined => {
                scoped.packages[target].state = CommittedState::Stopped;
                Ok(StageOutcome::Unchanged {
                    revision: scoped.active_revision,
                })
            }
            CommittedState::Stopped => Ok(StageOutcome::Unchanged {
                revision: scoped.active_revision,
            }),
        }
    }

    /// Prepare removal when active, or remove an inactive definition immediately.
    pub fn stage_undefine(&self, scope: &Scope, id: &str) -> Result<StageOutcome, RegistryError> {
        let mut scopes = self.write();
        let scoped = scopes
            .get_mut(scope)
            .ok_or_else(|| RegistryError::NotFound(id.to_owned()))?;
        reject_uncertain(scoped)?;
        reject_pending(scoped)?;
        let target = package_index(scoped, id)?;
        if scoped.packages[target].state == CommittedState::Running {
            return stage(scope, scoped, id, PendingKind::Undefine);
        }
        scoped.packages.remove(target);
        Ok(StageOutcome::Unchanged {
            revision: scoped.active_revision,
        })
    }

    /// Discard a desired mutation after candidate preparation or startup fails.
    pub fn abort(&self, transaction: &ExtensionTransaction) -> Result<(), RegistryError> {
        let mut scopes = self.write();
        let scoped =
            scopes
                .get_mut(&transaction.scope)
                .ok_or(RegistryError::TransactionMismatch {
                    revision: transaction.revision,
                })?;
        let matches = scoped
            .pending
            .as_ref()
            .is_some_and(|pending| pending.revision == transaction.revision);
        if !matches {
            return Err(RegistryError::TransactionMismatch {
                revision: transaction.revision,
            });
        }
        if scoped.transition_reserved {
            return Err(RegistryError::TransitionReserved);
        }
        scoped.pending = None;
        Ok(())
    }

    #[must_use]
    pub fn dynamic_statuses(&self, scope: &Scope) -> Vec<PackageStatus> {
        let scopes = self.read();
        let Some(scoped) = scopes.get(scope) else {
            return Vec::new();
        };
        scoped
            .packages
            .iter()
            .map(|entry| {
                let state = if scoped.uncertain.is_some() {
                    DynamicState::Uncertain
                } else {
                    scoped
                        .pending
                        .as_ref()
                        .filter(|pending| pending.id == entry.package.id)
                        .map_or_else(
                            || committed_state(entry.state),
                            |pending| match pending.kind {
                                PendingKind::Run => DynamicState::PendingRun,
                                PendingKind::Stop => DynamicState::PendingStop,
                                PendingKind::Undefine => DynamicState::PendingUndefine,
                            },
                        )
                };
                status(&entry.package, state)
            })
            .collect()
    }

    pub(crate) fn running(&self, scope: &Scope) -> Vec<Package> {
        self.read()
            .get(scope)
            .into_iter()
            .flat_map(|scoped| scoped.packages.iter())
            .filter(|entry| entry.state == CommittedState::Running)
            .map(|entry| entry.package.clone())
            .collect()
    }

    pub(crate) fn desired(&self, scope: &Scope) -> Vec<Package> {
        let scopes = self.read();
        let Some(scoped) = scopes.get(scope) else {
            return Vec::new();
        };
        scoped
            .packages
            .iter()
            .filter(|entry| match scoped.pending.as_ref() {
                Some(pending) if pending.id == entry.package.id => pending.kind == PendingKind::Run,
                _ => entry.state == CommittedState::Running,
            })
            .map(|entry| entry.package.clone())
            .collect()
    }

    fn read(&self) -> RwLockReadGuard<'_, BTreeMap<Scope, ScopeRegistry>> {
        self.scopes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, BTreeMap<Scope, ScopeRegistry>> {
        self.scopes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn commit_reserved(
        self: &Arc<Self>,
        transaction: &ExtensionTransaction,
    ) -> Result<CompositionLease, RegistryError> {
        let mut scopes = self.write();
        let scoped =
            scopes
                .get_mut(&transaction.scope)
                .ok_or(RegistryError::TransactionMismatch {
                    revision: transaction.revision,
                })?;
        reject_uncertain(scoped)?;
        if !scoped.transition_reserved {
            return Err(RegistryError::TransitionNotReserved);
        }
        let pending = scoped
            .pending
            .as_ref()
            .filter(|pending| pending.revision == transaction.revision)
            .cloned()
            .ok_or(RegistryError::TransactionMismatch {
                revision: transaction.revision,
            })?;
        let target = package_index(scoped, &pending.id)?;
        match pending.kind {
            PendingKind::Run => scoped.packages[target].state = CommittedState::Running,
            PendingKind::Stop => scoped.packages[target].state = CommittedState::Stopped,
            PendingKind::Undefine => {
                scoped.packages.remove(target);
            }
        }
        scoped.active_revision = pending.revision;
        scoped.pending = None;
        scoped.transition_reserved = false;
        scoped.active_consumers = 1;
        Ok(CompositionLease {
            registry: Arc::clone(self),
            scope: transaction.scope.clone(),
            revision: transaction.revision,
            released: false,
        })
    }

    fn abort_reserved(&self, transaction: &ExtensionTransaction) -> Result<(), RegistryError> {
        let mut scopes = self.write();
        let scoped =
            scopes
                .get_mut(&transaction.scope)
                .ok_or(RegistryError::TransactionMismatch {
                    revision: transaction.revision,
                })?;
        if !scoped.transition_reserved {
            return Err(RegistryError::TransitionNotReserved);
        }
        let matches = scoped
            .pending
            .as_ref()
            .is_some_and(|pending| pending.revision == transaction.revision);
        if !matches {
            return Err(RegistryError::TransactionMismatch {
                revision: transaction.revision,
            });
        }
        scoped.pending = None;
        scoped.transition_reserved = false;
        Ok(())
    }

    fn uncertain_reserved(&self, transaction: &ExtensionTransaction, message: String) {
        let mut scopes = self.write();
        if let Some(scoped) = scopes.get_mut(&transaction.scope)
            && scoped
                .pending
                .as_ref()
                .is_some_and(|pending| pending.revision == transaction.revision)
        {
            scoped.transition_reserved = false;
            scoped.uncertain = Some(message);
        }
    }

    fn release_lease(&self, scope: &Scope, revision: u64, uncertainty: Option<String>) {
        let mut scopes = self.write();
        let Some(scoped) = scopes.get_mut(scope) else {
            return;
        };
        if scoped.active_revision == revision && scoped.active_consumers > 0 {
            scoped.active_consumers -= 1;
        }
        if let Some(message) = uncertainty {
            scoped.uncertain = Some(message);
        }
    }
}

fn stage(
    scope: &Scope,
    scoped: &mut ScopeRegistry,
    id: &str,
    kind: PendingKind,
) -> Result<StageOutcome, RegistryError> {
    let revision = scoped
        .next_revision
        .checked_add(1)
        .ok_or(RegistryError::RevisionExhausted)?;
    scoped.next_revision = revision;
    scoped.pending = Some(PendingMutation {
        id: id.to_owned(),
        kind,
        revision,
    });
    Ok(StageOutcome::Pending(ExtensionTransaction {
        scope: scope.clone(),
        revision,
    }))
}

fn reject_pending(scoped: &ScopeRegistry) -> Result<(), RegistryError> {
    scoped.pending.as_ref().map_or(Ok(()), |pending| {
        Err(RegistryError::MutationPending {
            id: pending.id.clone(),
            revision: pending.revision,
        })
    })
}

fn reject_uncertain(scoped: &ScopeRegistry) -> Result<(), RegistryError> {
    scoped.uncertain.as_ref().map_or(Ok(()), |message| {
        Err(RegistryError::CompositionUncertain(message.clone()))
    })
}

fn package_index(scoped: &ScopeRegistry, id: &str) -> Result<usize, RegistryError> {
    scoped
        .packages
        .iter()
        .position(|entry| entry.package.id == id)
        .ok_or_else(|| RegistryError::NotFound(id.to_owned()))
}

const fn committed_state(state: CommittedState) -> DynamicState {
    match state {
        CommittedState::Defined => DynamicState::Defined,
        CommittedState::Running => DynamicState::Running,
        CommittedState::Stopped => DynamicState::Stopped,
    }
}

fn status(package: &Package, state: DynamicState) -> PackageStatus {
    PackageStatus {
        id: package.id.clone(),
        description: package.description.clone(),
        state,
        agents: package.agents.keys().map(str::to_owned).collect(),
        workflows: package.workflows.keys().map(str::to_owned).collect(),
        skills: package
            .skills
            .iter()
            .map(|skill| skill.name.clone())
            .collect(),
        tools: package.tools.keys().map(str::to_owned).collect(),
        runtime: package.runtime.as_ref().map(|runtime| match runtime {
            crate::PluginRuntime::Wasi { .. } => "wasi".to_owned(),
            crate::PluginRuntime::Process { .. } => "process".to_owned(),
        }),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error(transparent)]
    Manifest(#[from] crate::manifest::ManifestError),
    #[error("extension package `{0}` is already defined in this process")]
    AlreadyDefined(String),
    #[error("extension package `{0}` is not defined in this process")]
    NotFound(String),
    #[error(
        "process-local extension package `{0}` cannot declare executable runtime code; install it as a static package"
    )]
    DynamicExecutable(String),
    #[error("extension package activation failed")]
    Activation(#[source] Box<crate::resolve::ResolveError>),
    #[error(
        "extension package `{id}` already has pending composition revision {revision}; \
         acknowledge or abort it before preparing another mutation"
    )]
    MutationPending { id: String, revision: u64 },
    #[error("extension composition transaction {revision} is not pending")]
    TransactionMismatch { revision: u64 },
    #[error("extension composition revision mismatch: expected {expected}, got {actual}")]
    RevisionMismatch { expected: u64, actual: u64 },
    #[error("extension composition has {count} active consumer(s)")]
    ActiveConsumers { count: u64 },
    #[error("an extension composition transition is already reserved")]
    TransitionReserved,
    #[error("the extension composition transition was not reserved")]
    TransitionNotReserved,
    #[error("extension composition is uncertain: {0}")]
    CompositionUncertain(String),
    #[error("extension composition revision space is exhausted")]
    RevisionExhausted,
    #[error("extension composition consumer count is exhausted")]
    ConsumerCountExhausted,
}

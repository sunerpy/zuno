//! Scoped, transactional component assembly for Zuno harnesses.
//!
//! Components prepare typed services and deferred effects without changing the
//! outside world. The runtime stops the previous composition completely before
//! starting a candidate, publishes services only after every effect starts, and
//! records any cleanup outcome that cannot be proven stopped.

mod capability;

pub use capability::{
    CapabilityAvailability, CapabilityContract, CapabilityDefinitionError, CapabilityDescriptor,
    CapabilityKey, CapabilityProvenance, CapabilityScope, CapabilityVersion,
};

use async_trait::async_trait;
use futures::future::BoxFuture;
use std::any::{Any, TypeId, type_name};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(5);

type ErasedService = Arc<dyn Any + Send + Sync>;
type EffectStart = Box<dyn FnOnce() -> BoxFuture<'static, Result<EffectStop, EffectError>> + Send>;
type EffectStop = Box<dyn FnOnce() -> BoxFuture<'static, Result<(), EffectError>> + Send>;

/// A component that contributes typed services and deferred effects to one scope.
///
/// `prepare` must be side-effect free. Work that spawns, binds, subscribes, or
/// otherwise changes the outside world is registered through
/// [`PrepareContext::effect`] and starts only after the complete candidate has
/// prepared successfully.
#[async_trait]
pub trait Component: Send + Sync {
    /// Stable identity used for replacement, dependency ownership, and diagnostics.
    fn id(&self) -> &str;

    /// Prepare this component's services and deferred effects.
    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError>;
}

/// Runtime lifecycle tuning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeOptions {
    stop_timeout: Duration,
}

impl RuntimeOptions {
    /// Set the maximum wait for one effect disposer.
    ///
    /// # Panics
    ///
    /// Panics when `timeout` is zero because a zero cleanup deadline cannot prove
    /// that any asynchronous resource reached quiescence.
    #[must_use]
    pub fn with_stop_timeout(mut self, timeout: Duration) -> Self {
        assert!(!timeout.is_zero(), "effect stop timeout must be positive");
        self.stop_timeout = timeout;
        self
    }
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            stop_timeout: DEFAULT_STOP_TIMEOUT,
        }
    }
}

/// A deferred effect's start or stop failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct EffectError {
    message: String,
}

impl EffectError {
    /// Create one scrubbed lifecycle error.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Observable state of a runtime or component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    /// Candidate definitions are being prepared or their deferred effects are starting.
    Preparing,
    /// Services are published and effects are live.
    Active,
    /// Services have been withdrawn and effects are being stopped.
    Stopping,
    /// The scope contains no active component.
    Stopped,
    /// A deterministic lifecycle operation failed without an unknown side effect.
    Failed,
    /// The runtime cannot prove whether a side effect remains live.
    Uncertain,
    /// The scope has completed shutdown and cannot be mounted again.
    Closed,
}

impl fmt::Display for LifecycleState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Preparing => "preparing",
            Self::Active => "active",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
            Self::Uncertain => "uncertain",
            Self::Closed => "closed",
        })
    }
}

/// Lifecycle phase that produced a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecyclePhase {
    Prepare,
    Start,
    Stop,
    Restore,
}

impl fmt::Display for LifecyclePhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Prepare => "prepare",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restore => "restore",
        })
    }
}

/// Typed lifecycle failure classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleFailureKind {
    Rejected,
    Failed,
    TimedOut,
    Uncertain,
}

impl fmt::Display for LifecycleFailureKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Rejected => "rejected",
            Self::Failed => "failed",
            Self::TimedOut => "timed out",
            Self::Uncertain => "uncertain",
        })
    }
}

/// One lifecycle failure retained for clients and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleDiagnostic {
    pub component_id: String,
    pub effect_id: String,
    pub phase: LifecyclePhase,
    pub kind: LifecycleFailureKind,
    pub message: String,
}

/// Frontend-neutral component inventory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentSnapshot {
    pub id: String,
    pub state: LifecycleState,
    pub effects: Vec<String>,
    pub provides: Vec<String>,
    pub requires: Vec<String>,
}

/// Frontend-neutral runtime inventory and lifecycle diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSnapshot {
    pub name: String,
    pub state: LifecycleState,
    pub profile_id: Option<String>,
    pub components: Vec<ComponentSnapshot>,
    pub capabilities: Vec<CapabilityDescriptor>,
    pub diagnostics: Vec<LifecycleDiagnostic>,
}

/// An ordered group of components distributed as one profile unit.
pub struct ProfileBundle {
    id: String,
    components: Vec<Arc<dyn Component>>,
}

impl ProfileBundle {
    /// Create an empty bundle with a stable identifier.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            components: Vec::new(),
        }
    }

    /// Append one statically selected component.
    #[must_use]
    pub fn with_component<C>(mut self, component: C) -> Self
    where
        C: Component + 'static,
    {
        self.components.push(Arc::new(component));
        self
    }

    /// Append one dynamically selected component.
    #[must_use]
    pub fn with_shared(mut self, component: Arc<dyn Component>) -> Self {
        self.components.push(component);
        self
    }
}

/// The complete component composition selected for one harness profile.
pub struct HarnessProfile {
    id: String,
    bundles: Vec<ProfileBundle>,
}

impl HarnessProfile {
    /// Create an empty profile with a stable identifier.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            bundles: Vec::new(),
        }
    }

    /// Append one bundle in preparation order.
    #[must_use]
    pub fn with_bundle(mut self, bundle: ProfileBundle) -> Self {
        self.bundles.push(bundle);
        self
    }
}

#[derive(Clone)]
struct StagedService {
    key: TypeId,
    name: &'static str,
    value: ErasedService,
}

#[derive(Clone)]
struct OwnedService {
    owner: String,
    service: StagedService,
}

struct ServiceEntry {
    owner: String,
    value: ErasedService,
}

#[derive(Default)]
struct ScopeState {
    services: HashMap<TypeId, Vec<ServiceEntry>>,
    capabilities: HashMap<CapabilityKey, CapabilityDescriptor>,
    capability_generations: HashMap<CapabilityKey, u64>,
}

struct ScopeInner {
    name: String,
    parent: Option<Arc<ScopeInner>>,
    state: StdMutex<ScopeState>,
}

#[derive(Clone)]
struct Scope {
    inner: Arc<ScopeInner>,
}

impl Scope {
    fn root(name: String) -> Self {
        Self {
            inner: Arc::new(ScopeInner {
                name,
                parent: None,
                state: StdMutex::new(ScopeState::default()),
            }),
        }
    }

    fn child(&self, name: String) -> Self {
        Self {
            inner: Arc::new(ScopeInner {
                name,
                parent: Some(Arc::clone(&self.inner)),
                state: StdMutex::new(ScopeState::default()),
            }),
        }
    }

    fn name(&self) -> &str {
        &self.inner.name
    }

    fn service<T>(&self) -> Option<Arc<T>>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        self.service_with_owner::<T>(&HashSet::new())
            .map(|(_, service)| service)
    }

    fn service_with_owner<T>(
        &self,
        hidden_local_owners: &HashSet<String>,
    ) -> Option<(String, Arc<T>)>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        let local = {
            let state = self.inner.state.lock().expect("scope state poisoned");
            state.services.get(&TypeId::of::<T>()).and_then(|entries| {
                entries
                    .iter()
                    .rev()
                    .find(|entry| !hidden_local_owners.contains(&entry.owner))
                    .map(|entry| (entry.owner.clone(), Arc::clone(&entry.value)))
            })
        };
        if let Some((owner, value)) = local {
            return decode_service::<T>(&value).map(|service| (owner, service));
        }
        self.inner.parent.as_ref().and_then(|parent| {
            Scope {
                inner: Arc::clone(parent),
            }
            .service_with_owner::<T>(&HashSet::new())
        })
    }

    fn capability(&self, key: &CapabilityKey) -> Option<CapabilityDescriptor> {
        self.capability_with_owner(key, &HashSet::new())
    }

    fn capability_with_owner(
        &self,
        key: &CapabilityKey,
        hidden_local_owners: &HashSet<String>,
    ) -> Option<CapabilityDescriptor> {
        let local = {
            let state = self.inner.state.lock().expect("scope state poisoned");
            state
                .capabilities
                .get(key)
                .filter(|descriptor| !hidden_local_owners.contains(descriptor.owner()))
                .cloned()
        };
        local.or_else(|| {
            self.inner.parent.as_ref().and_then(|parent| {
                Scope {
                    inner: Arc::clone(parent),
                }
                .capability_with_owner(key, &HashSet::new())
            })
        })
    }

    fn next_capability_generation(&self, key: &CapabilityKey) -> Result<u64, RuntimeError> {
        self.inner
            .state
            .lock()
            .expect("scope state poisoned")
            .capability_generations
            .get(key)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| RuntimeError::CapabilityGenerationExhausted(key.clone()))
    }

    fn replace_all(
        &self,
        components: Vec<(String, Vec<StagedService>)>,
        capabilities: Vec<CapabilityDescriptor>,
    ) {
        let mut state = self.inner.state.lock().expect("scope state poisoned");
        state.services.clear();
        state.capabilities.clear();
        for (owner, services) in components {
            for service in services {
                state
                    .services
                    .entry(service.key)
                    .or_default()
                    .push(ServiceEntry {
                        owner: owner.clone(),
                        value: service.value,
                    });
            }
        }
        for descriptor in capabilities {
            state
                .capability_generations
                .insert(descriptor.key().clone(), descriptor.generation());
            state
                .capabilities
                .insert(descriptor.key().clone(), descriptor);
        }
    }

    fn clear(&self) {
        let mut state = self.inner.state.lock().expect("scope state poisoned");
        state.services.clear();
        state.capabilities.clear();
    }
}

fn erase_service<T>(service: Arc<T>) -> ErasedService
where
    T: ?Sized + Send + Sync + 'static,
{
    Arc::new(service)
}

fn decode_service<T>(service: &ErasedService) -> Option<Arc<T>>
where
    T: ?Sized + Send + Sync + 'static,
{
    service.downcast_ref::<Arc<T>>().cloned()
}

struct PreparedEffect {
    id: String,
    start: EffectStart,
}

struct ActiveEffect {
    id: String,
    stop: EffectStop,
}

#[derive(Clone)]
struct Requirement {
    owner: String,
    key: RequirementKey,
}

#[derive(Clone)]
enum RequirementKey {
    Typed(&'static str),
    Named { key: CapabilityKey, generation: u64 },
}

impl fmt::Display for RequirementKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Typed(service) => formatter.write_str(service),
            Self::Named { key, generation } => {
                write!(formatter, "{key} generation {generation}")
            }
        }
    }
}

/// Side-effect-free context passed to one component preparation.
pub struct PrepareContext {
    component_id: String,
    scope: Scope,
    candidate_services: Vec<OwnedService>,
    candidate_capabilities: Vec<CapabilityDescriptor>,
    hidden_owners: Arc<HashSet<String>>,
    services: Vec<StagedService>,
    capabilities: Vec<CapabilityDescriptor>,
    effects: Vec<PreparedEffect>,
    requirements: Vec<Requirement>,
}

impl PrepareContext {
    fn new(
        component_id: String,
        scope: Scope,
        candidate_services: Vec<OwnedService>,
        candidate_capabilities: Vec<CapabilityDescriptor>,
        hidden_owners: Arc<HashSet<String>>,
    ) -> Self {
        Self {
            component_id,
            scope,
            candidate_services,
            candidate_capabilities,
            hidden_owners,
            services: Vec::new(),
            capabilities: Vec::new(),
            effects: Vec::new(),
            requirements: Vec::new(),
        }
    }

    /// Stage one typed service.
    ///
    /// A component may contribute at most one service for each Rust type.
    pub fn provide<T>(&mut self, service: Arc<T>) -> Result<(), RuntimeError>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        let key = TypeId::of::<T>();
        let name = type_name::<T>();
        if self.services.iter().any(|candidate| candidate.key == key) {
            return Err(RuntimeError::DuplicateService(name));
        }
        self.services.push(StagedService {
            key,
            name,
            value: erase_service(service),
        });
        Ok(())
    }

    /// Resolve and record one required service.
    pub fn require<T>(&mut self) -> Result<Arc<T>, RuntimeError>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        let key = TypeId::of::<T>();
        let name = type_name::<T>();
        let own = self
            .services
            .iter()
            .rev()
            .find(|service| service.key == key)
            .and_then(|service| {
                decode_service::<T>(&service.value).map(|value| (self.component_id.clone(), value))
            });
        let candidate = own.or_else(|| {
            self.candidate_services
                .iter()
                .rev()
                .find(|service| service.service.key == key)
                .and_then(|service| {
                    decode_service::<T>(&service.service.value)
                        .map(|value| (service.owner.clone(), value))
                })
        });
        let resolved = candidate.or_else(|| {
            self.scope
                .service_with_owner::<T>(self.hidden_owners.as_ref())
        });
        let (owner, service) = resolved.ok_or(RuntimeError::MissingService(name))?;
        self.requirements.push(Requirement {
            owner,
            key: RequirementKey::Typed(name),
        });
        Ok(service)
    }

    /// Stage one runtime-named capability descriptor.
    ///
    /// The descriptor contains no executable value. Native providers continue to
    /// expose their executable interface through [`Self::provide`], while dynamic
    /// consumers resolve this stable identity, contract, generation, and provenance.
    pub fn provide_capability(
        &mut self,
        key: CapabilityKey,
        contract: CapabilityContract,
        provenance: CapabilityProvenance,
    ) -> Result<(), RuntimeError> {
        if self
            .capabilities
            .iter()
            .chain(self.candidate_capabilities.iter())
            .any(|candidate| candidate.key() == &key)
        {
            return Err(RuntimeError::DuplicateCapability(key));
        }
        let generation = self.scope.next_capability_generation(&key)?;
        self.capabilities.push(CapabilityDescriptor::available(
            key,
            self.component_id.clone(),
            self.scope.name().to_owned(),
            generation,
            contract,
            provenance,
        ));
        Ok(())
    }

    /// Resolve and record one runtime-named capability generation.
    pub fn require_capability(
        &mut self,
        key: &CapabilityKey,
    ) -> Result<CapabilityDescriptor, RuntimeError> {
        let own = self
            .capabilities
            .iter()
            .rev()
            .find(|candidate| candidate.key() == key)
            .cloned();
        let candidate = own.or_else(|| {
            self.candidate_capabilities
                .iter()
                .rev()
                .find(|candidate| candidate.key() == key)
                .cloned()
        });
        let descriptor = candidate
            .or_else(|| {
                self.scope
                    .capability_with_owner(key, self.hidden_owners.as_ref())
            })
            .ok_or_else(|| RuntimeError::MissingCapability(key.clone()))?;
        self.requirements.push(Requirement {
            owner: descriptor.owner().to_owned(),
            key: RequirementKey::Named {
                key: key.clone(),
                generation: descriptor.generation(),
            },
        });
        Ok(descriptor)
    }

    /// Register a deferred side effect.
    ///
    /// `start` runs only after every candidate component has prepared. It must
    /// either return the exact disposer for the acquired resource or return an
    /// error without leaving a live resource.
    pub fn effect<Start, StartFuture, Stop, StopFuture>(
        &mut self,
        id: impl Into<String>,
        start: Start,
    ) -> Result<(), RuntimeError>
    where
        Start: FnOnce() -> StartFuture + Send + 'static,
        StartFuture: Future<Output = Result<Stop, EffectError>> + Send + 'static,
        Stop: FnOnce() -> StopFuture + Send + 'static,
        StopFuture: Future<Output = Result<(), EffectError>> + Send + 'static,
    {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(RuntimeError::EmptyEffectId(self.component_id.clone()));
        }
        if self.effects.iter().any(|effect| effect.id == id) {
            return Err(RuntimeError::DuplicateEffect {
                component: self.component_id.clone(),
                effect: id,
            });
        }
        self.effects.push(PreparedEffect {
            id,
            start: Box::new(move || {
                Box::pin(async move {
                    let stop = start().await?;
                    let stop: EffectStop =
                        Box::new(move || -> BoxFuture<'static, Result<(), EffectError>> {
                            Box::pin(stop())
                        });
                    Ok(stop)
                })
            }),
        });
        Ok(())
    }

    fn into_parts(
        self,
    ) -> (
        Vec<StagedService>,
        Vec<CapabilityDescriptor>,
        Vec<PreparedEffect>,
        Vec<Requirement>,
    ) {
        (
            self.services,
            self.capabilities,
            self.effects,
            self.requirements,
        )
    }
}

#[derive(Clone)]
struct ComponentDefinition {
    id: String,
    component: Arc<dyn Component>,
}

#[derive(Clone)]
struct ProfileDefinition {
    id: String,
    components: Vec<ComponentDefinition>,
}

#[derive(Clone, Default)]
struct CompositionDefinition {
    profile: Option<ProfileDefinition>,
    mounts: Vec<ComponentDefinition>,
}

impl CompositionDefinition {
    fn components(&self) -> Vec<ComponentDefinition> {
        self.profile
            .iter()
            .flat_map(|profile| profile.components.iter().cloned())
            .chain(self.mounts.iter().cloned())
            .collect()
    }

    fn component_ids(&self) -> HashSet<String> {
        self.components()
            .into_iter()
            .map(|component| component.id)
            .collect()
    }

    fn profile_id(&self) -> Option<String> {
        self.profile.as_ref().map(|profile| profile.id.clone())
    }
}

struct PreparedComponent {
    definition: ComponentDefinition,
    services: Vec<StagedService>,
    capabilities: Vec<CapabilityDescriptor>,
    effects: Vec<PreparedEffect>,
    requirements: Vec<Requirement>,
}

struct PreparedComposition {
    definition: CompositionDefinition,
    components: Vec<PreparedComponent>,
}

struct ActiveComponent {
    definition: ComponentDefinition,
    effects: Vec<ActiveEffect>,
    provides: Vec<String>,
    requirements: Vec<Requirement>,
}

impl ActiveComponent {
    fn snapshot(&self, state: LifecycleState) -> ComponentSnapshot {
        ComponentSnapshot {
            id: self.definition.id.clone(),
            state,
            effects: self
                .effects
                .iter()
                .map(|effect| effect.id.clone())
                .collect(),
            provides: self.provides.iter().map(ToOwned::to_owned).collect(),
            requires: self
                .requirements
                .iter()
                .map(|requirement| format!("{} <- {}", requirement.key, requirement.owner))
                .collect(),
        }
    }
}

struct StartedComposition {
    definition: CompositionDefinition,
    active: Vec<ActiveComponent>,
    services: Vec<(String, Vec<StagedService>)>,
    capabilities: Vec<CapabilityDescriptor>,
}

struct RuntimeState {
    phase: LifecycleState,
    definition: CompositionDefinition,
    active: Vec<ActiveComponent>,
    components: Vec<ComponentSnapshot>,
    capabilities: Vec<CapabilityDescriptor>,
    diagnostics: Vec<LifecycleDiagnostic>,
    children: Vec<Weak<HarnessRuntimeInner>>,
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            phase: LifecycleState::Stopped,
            definition: CompositionDefinition::default(),
            active: Vec::new(),
            components: Vec::new(),
            capabilities: Vec::new(),
            diagnostics: Vec::new(),
            children: Vec::new(),
        }
    }
}

struct HarnessRuntimeInner {
    scope: Scope,
    operation: Arc<AsyncMutex<()>>,
    state: StdMutex<RuntimeState>,
    options: RuntimeOptions,
}

/// One independently configurable harness runtime.
#[derive(Clone)]
pub struct HarnessRuntime {
    inner: Arc<HarnessRuntimeInner>,
}

impl HarnessRuntime {
    /// Create one empty root runtime with default lifecycle timeouts.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self::with_options(name, RuntimeOptions::default())
    }

    /// Create one empty root runtime with explicit lifecycle options.
    #[must_use]
    pub fn with_options(name: impl Into<String>, options: RuntimeOptions) -> Self {
        Self {
            inner: Arc::new(HarnessRuntimeInner {
                scope: Scope::root(name.into()),
                operation: Arc::new(AsyncMutex::new(())),
                state: StdMutex::new(RuntimeState::default()),
                options,
            }),
        }
    }

    /// Return this runtime scope's diagnostic name.
    #[must_use]
    pub fn name(&self) -> &str {
        self.inner.scope.name()
    }

    /// Return a stable inventory and the latest lifecycle diagnostics.
    #[must_use]
    pub fn snapshot(&self) -> RuntimeSnapshot {
        let state = self.inner.state.lock().expect("runtime state poisoned");
        RuntimeSnapshot {
            name: self.name().to_owned(),
            state: state.phase,
            profile_id: state.definition.profile_id(),
            components: state.components.clone(),
            capabilities: state.capabilities.clone(),
            diagnostics: state.diagnostics.clone(),
        }
    }

    /// Create an isolated child scope that inherits parent services.
    ///
    /// Shutting down the parent also shuts down all live children in reverse
    /// creation order.
    #[must_use]
    pub fn child(&self, name: impl Into<String>) -> Self {
        let scope = self.inner.scope.child(name.into());
        let mut parent = self.inner.state.lock().expect("runtime state poisoned");
        let closed = parent.phase == LifecycleState::Closed;
        let child = Arc::new(HarnessRuntimeInner {
            scope,
            operation: Arc::clone(&self.inner.operation),
            state: StdMutex::new(RuntimeState {
                phase: if closed {
                    LifecycleState::Closed
                } else {
                    LifecycleState::Stopped
                },
                ..RuntimeState::default()
            }),
            options: self.inner.options,
        });
        if !closed {
            parent.children.push(Arc::downgrade(&child));
        }
        drop(parent);
        Self { inner: child }
    }

    /// Resolve the nearest service in this runtime's scope chain.
    #[must_use]
    pub fn service<T>(&self) -> Option<Arc<T>>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        self.inner.scope.service::<T>()
    }

    /// Resolve the nearest named capability in this runtime's scope chain.
    #[must_use]
    pub fn capability(&self, key: &CapabilityKey) -> Option<CapabilityDescriptor> {
        self.inner.scope.capability(key)
    }

    /// Whether a previously resolved descriptor is still the routable generation.
    #[must_use]
    pub fn capability_is_current(&self, descriptor: &CapabilityDescriptor) -> bool {
        descriptor.availability() == CapabilityAvailability::Available
            && self.capability(descriptor.key()).as_ref() == Some(descriptor)
    }

    /// Return the active profile identifier.
    #[must_use]
    pub fn active_profile_id(&self) -> Option<String> {
        self.inner
            .state
            .lock()
            .expect("runtime state poisoned")
            .definition
            .profile_id()
    }

    /// Prepare and activate one complete harness profile.
    ///
    /// Existing local mounts are re-prepared against the candidate profile so a
    /// provider change reconnects consumers before services become visible.
    pub async fn activate_profile(&self, profile: HarnessProfile) -> Result<(), RuntimeError> {
        let _operation = self.inner.operation.lock().await;
        let profile = validate_profile(profile)?;
        let mut candidate = self.current_definition()?;
        let mount_ids = candidate
            .mounts
            .iter()
            .map(|component| component.id.as_str())
            .collect::<HashSet<_>>();
        if let Some(duplicate) = profile
            .components
            .iter()
            .find(|component| mount_ids.contains(component.id.as_str()))
        {
            return Err(RuntimeError::DuplicateComponent(duplicate.id.clone()));
        }
        candidate.profile = Some(profile);
        self.transition_to(candidate).await
    }

    /// Add one component and recompose the local scope transactionally.
    pub async fn mount<C>(&self, component: C) -> Result<(), RuntimeError>
    where
        C: Component + 'static,
    {
        self.mount_shared(Arc::new(component)).await
    }

    /// Add one dynamically selected component.
    pub async fn mount_shared(&self, component: Arc<dyn Component>) -> Result<(), RuntimeError> {
        let _operation = self.inner.operation.lock().await;
        let id = validate_component_id(component.id())?;
        let mut candidate = self.current_definition()?;
        if candidate.component_ids().contains(&id) {
            return Err(RuntimeError::DuplicateComponent(id));
        }
        candidate.mounts.push(ComponentDefinition { id, component });
        self.transition_to(candidate).await
    }

    /// Replace a local component and re-prepare all consumers in this scope.
    pub async fn replace<C>(&self, component: C) -> Result<(), RuntimeError>
    where
        C: Component + 'static,
    {
        self.replace_shared(Arc::new(component)).await
    }

    /// Replace one dynamically selected local component.
    pub async fn replace_shared(&self, component: Arc<dyn Component>) -> Result<(), RuntimeError> {
        let _operation = self.inner.operation.lock().await;
        let id = validate_component_id(component.id())?;
        let mut candidate = self.current_definition()?;
        let position = candidate
            .mounts
            .iter()
            .position(|mounted| mounted.id == id)
            .ok_or_else(|| RuntimeError::ComponentNotMounted(id.clone()))?;
        candidate.mounts[position] = ComponentDefinition { id, component };
        self.transition_to(candidate).await
    }

    /// Remove one local component and re-prepare consumers against revealed services.
    pub async fn unmount(&self, id: &str) -> Result<(), RuntimeError> {
        let _operation = self.inner.operation.lock().await;
        let mut candidate = self.current_definition()?;
        let position = candidate
            .mounts
            .iter()
            .position(|mounted| mounted.id == id)
            .ok_or_else(|| RuntimeError::ComponentNotMounted(id.to_owned()))?;
        candidate.mounts.remove(position);
        self.transition_to(candidate).await
    }

    /// Close this runtime and its descendants.
    ///
    /// Children close in reverse creation order, then local components close in
    /// reverse preparation order. A successful repeated shutdown is a no-op.
    pub async fn shutdown(&self) -> Result<(), RuntimeError> {
        let _operation = self.inner.operation.lock().await;
        let failures = shutdown_inner(Arc::clone(&self.inner)).await;
        failures
            .into_iter()
            .next()
            .map_or(Ok(()), |diagnostic| Err(lifecycle_error(diagnostic)))
    }

    fn current_definition(&self) -> Result<CompositionDefinition, RuntimeError> {
        let state = self.inner.state.lock().expect("runtime state poisoned");
        match state.phase {
            LifecycleState::Closed => Err(RuntimeError::Closed),
            LifecycleState::Failed | LifecycleState::Uncertain => {
                Err(RuntimeError::NotOperational(state.phase))
            }
            LifecycleState::Preparing
            | LifecycleState::Stopping
            | LifecycleState::Active
            | LifecycleState::Stopped => Ok(state.definition.clone()),
        }
    }

    async fn transition_to(&self, candidate: CompositionDefinition) -> Result<(), RuntimeError> {
        if let Some((scope, state)) = first_non_quiescent_descendant(&self.inner) {
            return Err(RuntimeError::LiveChildScope { scope, state });
        }
        let previous_definition = {
            let mut state = self.inner.state.lock().expect("runtime state poisoned");
            let previous = state.definition.clone();
            state.phase = LifecycleState::Preparing;
            state.components = definition_snapshots(&candidate, LifecycleState::Preparing);
            previous
        };

        let prepared =
            match prepare_composition(&self.inner.scope, &previous_definition, candidate).await {
                Ok(prepared) => prepared,
                Err(error) => {
                    self.restore_stable_projection();
                    return Err(error);
                }
            };

        let previous_active = {
            let mut state = self.inner.state.lock().expect("runtime state poisoned");
            state.phase = LifecycleState::Stopping;
            state.components = state
                .active
                .iter()
                .map(|component| component.snapshot(LifecycleState::Stopping))
                .collect();
            for descriptor in &mut state.capabilities {
                descriptor.withdraw();
            }
            self.inner.scope.clear();
            std::mem::take(&mut state.active)
        };
        let previous_snapshots = previous_active
            .iter()
            .map(|component| component.snapshot(LifecycleState::Uncertain))
            .collect::<Vec<_>>();
        let stop_failures = stop_components(previous_active, self.inner.options.stop_timeout).await;
        if !stop_failures.is_empty() {
            self.set_unresolved(previous_snapshots, stop_failures.clone());
            return Err(lifecycle_error(stop_failures[0].clone()));
        }

        {
            let mut state = self.inner.state.lock().expect("runtime state poisoned");
            state.phase = LifecycleState::Preparing;
            state.components =
                definition_snapshots(&prepared.definition, LifecycleState::Preparing);
        }
        match start_composition(prepared, self.inner.options.stop_timeout).await {
            Ok(started) => {
                self.publish(started);
                Ok(())
            }
            Err(failure) if !failure.cleanup_failures.is_empty() => {
                let snapshots =
                    definition_snapshots(&failure.definition, LifecycleState::Uncertain);
                let mut diagnostics = vec![failure.diagnostic.clone()];
                diagnostics.extend(failure.cleanup_failures);
                self.set_unresolved(snapshots, diagnostics);
                Err(lifecycle_error(failure.diagnostic))
            }
            Err(failure) => {
                let candidate_error = lifecycle_error(failure.diagnostic.clone());
                let restore = prepare_composition(
                    &self.inner.scope,
                    &CompositionDefinition::default(),
                    previous_definition.clone(),
                )
                .await;
                let restored = match restore {
                    Ok(prepared) => {
                        start_composition(prepared, self.inner.options.stop_timeout).await
                    }
                    Err(error) => {
                        self.set_failed(
                            &previous_definition,
                            vec![
                                failure.diagnostic,
                                LifecycleDiagnostic {
                                    component_id: String::from("<composition>"),
                                    effect_id: String::new(),
                                    phase: LifecyclePhase::Restore,
                                    kind: LifecycleFailureKind::Failed,
                                    message: error.to_string(),
                                },
                            ],
                        );
                        return Err(RuntimeError::RestoreFailed {
                            candidate: Box::new(candidate_error),
                            restore: Box::new(error),
                        });
                    }
                };
                match restored {
                    Ok(started) => {
                        self.publish_with_diagnostic(started, failure.diagnostic);
                        Err(candidate_error)
                    }
                    Err(restore_failure) => {
                        let mut diagnostics = vec![failure.diagnostic];
                        diagnostics.push(LifecycleDiagnostic {
                            component_id: restore_failure.diagnostic.component_id.clone(),
                            effect_id: restore_failure.diagnostic.effect_id.clone(),
                            phase: LifecyclePhase::Restore,
                            kind: restore_failure.diagnostic.kind,
                            message: restore_failure.diagnostic.message.clone(),
                        });
                        diagnostics.extend(restore_failure.cleanup_failures);
                        self.set_failed(&previous_definition, diagnostics);
                        Err(RuntimeError::RestoreFailed {
                            candidate: Box::new(candidate_error),
                            restore: Box::new(lifecycle_error(restore_failure.diagnostic)),
                        })
                    }
                }
            }
        }
    }

    fn publish(&self, started: StartedComposition) {
        self.publish_inner(started, None);
    }

    fn publish_with_diagnostic(
        &self,
        started: StartedComposition,
        diagnostic: LifecycleDiagnostic,
    ) {
        self.publish_inner(started, Some(diagnostic));
    }

    fn publish_inner(&self, started: StartedComposition, diagnostic: Option<LifecycleDiagnostic>) {
        let StartedComposition {
            definition,
            active,
            services,
            capabilities,
        } = started;
        self.inner.scope.replace_all(services, capabilities.clone());
        let mut state = self.inner.state.lock().expect("runtime state poisoned");
        state.definition = definition;
        state.components = active
            .iter()
            .map(|component| component.snapshot(LifecycleState::Active))
            .collect();
        state.phase = if state.components.is_empty() {
            LifecycleState::Stopped
        } else {
            LifecycleState::Active
        };
        state.active = active;
        state.capabilities = capabilities;
        if let Some(diagnostic) = diagnostic {
            state.diagnostics.push(diagnostic);
        }
    }

    fn restore_stable_projection(&self) {
        let mut state = self.inner.state.lock().expect("runtime state poisoned");
        state.components = state
            .active
            .iter()
            .map(|component| component.snapshot(LifecycleState::Active))
            .collect();
        state.phase = if state.active.is_empty() {
            LifecycleState::Stopped
        } else {
            LifecycleState::Active
        };
    }

    fn set_unresolved(
        &self,
        components: Vec<ComponentSnapshot>,
        diagnostics: Vec<LifecycleDiagnostic>,
    ) {
        let mut state = self.inner.state.lock().expect("runtime state poisoned");
        state.active.clear();
        state.components = components;
        for descriptor in &mut state.capabilities {
            descriptor.withdraw();
        }
        state.phase = LifecycleState::Uncertain;
        state.diagnostics.extend(diagnostics);
    }

    fn set_failed(
        &self,
        definition: &CompositionDefinition,
        diagnostics: Vec<LifecycleDiagnostic>,
    ) {
        let mut state = self.inner.state.lock().expect("runtime state poisoned");
        state.active.clear();
        state.components = definition_snapshots(definition, LifecycleState::Failed);
        for descriptor in &mut state.capabilities {
            descriptor.withdraw();
        }
        state.phase = LifecycleState::Failed;
        state.diagnostics.extend(diagnostics);
    }
}

fn first_non_quiescent_descendant(
    inner: &Arc<HarnessRuntimeInner>,
) -> Option<(String, LifecycleState)> {
    let children = inner
        .state
        .lock()
        .expect("runtime state poisoned")
        .children
        .iter()
        .filter_map(Weak::upgrade)
        .collect::<Vec<_>>();
    for child in children {
        let (name, phase, has_active) = {
            let state = child.state.lock().expect("runtime state poisoned");
            (
                child.scope.name().to_owned(),
                state.phase,
                !state.active.is_empty(),
            )
        };
        if has_active
            || matches!(
                phase,
                LifecycleState::Preparing
                    | LifecycleState::Active
                    | LifecycleState::Stopping
                    | LifecycleState::Failed
                    | LifecycleState::Uncertain
            )
        {
            return Some((name, phase));
        }
        if let Some(descendant) = first_non_quiescent_descendant(&child) {
            return Some(descendant);
        }
    }
    None
}

async fn prepare_composition(
    scope: &Scope,
    previous: &CompositionDefinition,
    definition: CompositionDefinition,
) -> Result<PreparedComposition, RuntimeError> {
    let hidden_owners = Arc::new(previous.component_ids());
    let mut candidate_services = Vec::<OwnedService>::new();
    let mut candidate_capabilities = Vec::<CapabilityDescriptor>::new();
    let mut prepared = Vec::new();
    for component in definition.components() {
        let mut context = PrepareContext::new(
            component.id.clone(),
            scope.clone(),
            candidate_services.clone(),
            candidate_capabilities.clone(),
            Arc::clone(&hidden_owners),
        );
        component.component.prepare(&mut context).await?;
        let (services, capabilities, effects, requirements) = context.into_parts();
        candidate_services.extend(services.iter().cloned().map(|service| OwnedService {
            owner: component.id.clone(),
            service,
        }));
        candidate_capabilities.extend(capabilities.iter().cloned());
        prepared.push(PreparedComponent {
            definition: component,
            services,
            capabilities,
            effects,
            requirements,
        });
    }
    Ok(PreparedComposition {
        definition,
        components: prepared,
    })
}

struct StartFailure {
    definition: CompositionDefinition,
    diagnostic: LifecycleDiagnostic,
    cleanup_failures: Vec<LifecycleDiagnostic>,
}

async fn start_composition(
    prepared: PreparedComposition,
    stop_timeout: Duration,
) -> Result<StartedComposition, Box<StartFailure>> {
    let definition = prepared.definition;
    let mut active = Vec::<ActiveComponent>::new();
    let mut services = Vec::new();
    let mut capabilities = Vec::new();
    for component in prepared.components {
        let PreparedComponent {
            definition: component_definition,
            services: component_services,
            capabilities: component_capabilities,
            effects,
            requirements,
        } = component;
        let mut active_effects = Vec::new();
        for effect in effects {
            let effect_id = effect.id.clone();
            match (effect.start)().await {
                Ok(stop) => active_effects.push(ActiveEffect {
                    id: effect.id,
                    stop,
                }),
                Err(error) => {
                    active.push(ActiveComponent {
                        definition: component_definition.clone(),
                        effects: active_effects,
                        provides: provided_names(&component_services, &component_capabilities),
                        requirements: requirements.clone(),
                    });
                    let cleanup_failures = stop_components(active, stop_timeout).await;
                    return Err(Box::new(StartFailure {
                        definition,
                        diagnostic: LifecycleDiagnostic {
                            component_id: component_definition.id,
                            effect_id,
                            phase: LifecyclePhase::Start,
                            kind: LifecycleFailureKind::Failed,
                            message: error.to_string(),
                        },
                        cleanup_failures,
                    }));
                }
            }
        }
        services.push((component_definition.id.clone(), component_services.clone()));
        capabilities.extend(component_capabilities.iter().cloned());
        active.push(ActiveComponent {
            definition: component_definition,
            effects: active_effects,
            provides: provided_names(&component_services, &component_capabilities),
            requirements,
        });
    }
    Ok(StartedComposition {
        definition,
        active,
        services,
        capabilities,
    })
}

fn provided_names(
    services: &[StagedService],
    capabilities: &[CapabilityDescriptor],
) -> Vec<String> {
    services
        .iter()
        .map(|service| service.name.to_owned())
        .chain(capabilities.iter().map(|descriptor| {
            format!(
                "{} generation {}",
                descriptor.key(),
                descriptor.generation()
            )
        }))
        .collect()
}

async fn stop_components(
    mut components: Vec<ActiveComponent>,
    timeout: Duration,
) -> Vec<LifecycleDiagnostic> {
    let mut diagnostics = Vec::new();
    while let Some(mut component) = components.pop() {
        while let Some(effect) = component.effects.pop() {
            let effect_id = effect.id;
            match tokio::time::timeout(timeout, (effect.stop)()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => diagnostics.push(LifecycleDiagnostic {
                    component_id: component.definition.id.clone(),
                    effect_id,
                    phase: LifecyclePhase::Stop,
                    kind: LifecycleFailureKind::Uncertain,
                    message: error.to_string(),
                }),
                Err(_elapsed) => diagnostics.push(LifecycleDiagnostic {
                    component_id: component.definition.id.clone(),
                    effect_id,
                    phase: LifecyclePhase::Stop,
                    kind: LifecycleFailureKind::TimedOut,
                    message: format!("effect stop timed out after {} ms", timeout.as_millis()),
                }),
            }
        }
    }
    diagnostics
}

fn shutdown_inner(inner: Arc<HarnessRuntimeInner>) -> BoxFuture<'static, Vec<LifecycleDiagnostic>> {
    Box::pin(async move {
        let (children, previous_phase, previous_diagnostics) = {
            let mut state = inner.state.lock().expect("runtime state poisoned");
            if state.phase == LifecycleState::Closed {
                return Vec::new();
            }
            let previous_phase = state.phase;
            let previous_diagnostics = if matches!(
                previous_phase,
                LifecycleState::Failed | LifecycleState::Uncertain
            ) {
                state.diagnostics.clone()
            } else {
                Vec::new()
            };
            if previous_diagnostics.is_empty() {
                state.phase = LifecycleState::Stopping;
                state.components = state
                    .active
                    .iter()
                    .map(|component| component.snapshot(LifecycleState::Stopping))
                    .collect();
                for descriptor in &mut state.capabilities {
                    descriptor.withdraw();
                }
            }
            (
                std::mem::take(&mut state.children),
                previous_phase,
                previous_diagnostics,
            )
        };

        let mut new_diagnostics = Vec::new();
        for child in children
            .into_iter()
            .rev()
            .filter_map(|child| child.upgrade())
        {
            new_diagnostics.extend(shutdown_inner(child).await);
        }

        inner.scope.clear();
        let active = {
            let mut state = inner.state.lock().expect("runtime state poisoned");
            std::mem::take(&mut state.active)
        };
        let local_failures = stop_components(active, inner.options.stop_timeout).await;
        new_diagnostics.extend(local_failures.iter().cloned());

        let mut state = inner.state.lock().expect("runtime state poisoned");
        state.definition = CompositionDefinition::default();
        if matches!(
            previous_phase,
            LifecycleState::Failed | LifecycleState::Uncertain
        ) {
            state.phase = previous_phase;
        } else if new_diagnostics.is_empty() {
            state.phase = LifecycleState::Closed;
            state.components.clear();
            state.capabilities.clear();
        } else {
            state.phase = LifecycleState::Uncertain;
            if local_failures.is_empty() {
                state.components.clear();
            } else {
                for component in &mut state.components {
                    component.state = LifecycleState::Uncertain;
                }
            }
        }
        state.diagnostics.extend(new_diagnostics.iter().cloned());
        let mut diagnostics = previous_diagnostics;
        diagnostics.extend(new_diagnostics);
        diagnostics
    })
}

fn definition_snapshots(
    definition: &CompositionDefinition,
    state: LifecycleState,
) -> Vec<ComponentSnapshot> {
    definition
        .components()
        .into_iter()
        .map(|component| ComponentSnapshot {
            id: component.id,
            state,
            effects: Vec::new(),
            provides: Vec::new(),
            requires: Vec::new(),
        })
        .collect()
}

fn validate_profile(profile: HarnessProfile) -> Result<ProfileDefinition, RuntimeError> {
    let id = profile.id.trim();
    if id.is_empty() {
        return Err(RuntimeError::EmptyProfileId);
    }
    if profile.bundles.is_empty() {
        return Err(RuntimeError::EmptyProfile(id.to_owned()));
    }

    let mut bundle_ids = HashSet::new();
    let mut component_ids = HashSet::new();
    let mut components = Vec::new();
    for bundle in profile.bundles {
        let bundle_id = bundle.id.trim();
        if bundle_id.is_empty() {
            return Err(RuntimeError::EmptyBundleId);
        }
        if !bundle_ids.insert(bundle_id.to_owned()) {
            return Err(RuntimeError::DuplicateBundle(bundle_id.to_owned()));
        }
        if bundle.components.is_empty() {
            return Err(RuntimeError::EmptyBundle(bundle_id.to_owned()));
        }
        for component in bundle.components {
            let component_id = validate_component_id(component.id())?;
            if !component_ids.insert(component_id.clone()) {
                return Err(RuntimeError::DuplicateComponent(component_id));
            }
            components.push(ComponentDefinition {
                id: component_id,
                component,
            });
        }
    }
    Ok(ProfileDefinition {
        id: id.to_owned(),
        components,
    })
}

fn validate_component_id(id: &str) -> Result<String, RuntimeError> {
    if id.trim().is_empty() {
        Err(RuntimeError::EmptyComponentId)
    } else {
        Ok(id.to_owned())
    }
}

fn lifecycle_error(diagnostic: LifecycleDiagnostic) -> RuntimeError {
    RuntimeError::Lifecycle {
        component: diagnostic.component_id,
        effect: diagnostic.effect_id,
        phase: diagnostic.phase,
        kind: diagnostic.kind,
        message: diagnostic.message,
    }
}

/// Load-time and lifecycle failures from the harness runtime.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RuntimeError {
    /// A component rejected its own configuration or dependencies.
    #[error("component failed: {0}")]
    Component(String),
    /// A required typed service was absent.
    #[error("required service `{0}` is not registered")]
    MissingService(&'static str),
    /// One component tried to register the same typed service twice.
    #[error("service `{0}` is already staged by this component")]
    DuplicateService(&'static str),
    /// A required runtime-named capability was absent.
    #[error("required capability `{0}` is not registered")]
    MissingCapability(CapabilityKey),
    /// One local candidate declared the same runtime-named capability twice.
    #[error("capability `{0}` is already staged in this runtime scope")]
    DuplicateCapability(CapabilityKey),
    /// A scope exhausted the monotonic generation counter for one capability.
    #[error("capability `{0}` exhausted its generation counter")]
    CapabilityGenerationExhausted(CapabilityKey),
    /// One component registered the same effect id twice.
    #[error("component `{component}` registered effect `{effect}` more than once")]
    DuplicateEffect { component: String, effect: String },
    /// An effect identifier was empty.
    #[error("component `{0}` registered an empty effect id")]
    EmptyEffectId(String),
    /// A component id was mounted twice.
    #[error("component `{0}` is already mounted")]
    DuplicateComponent(String),
    /// Replacement or unmount named no local mounted component.
    #[error("component `{0}` is not mounted")]
    ComponentNotMounted(String),
    /// A component id must be non-empty.
    #[error("component id must not be empty")]
    EmptyComponentId,
    /// A profile id must be non-empty.
    #[error("profile id must not be empty")]
    EmptyProfileId,
    /// A profile must contain at least one bundle.
    #[error("profile `{0}` must contain at least one bundle")]
    EmptyProfile(String),
    /// A bundle id must be non-empty.
    #[error("bundle id must not be empty")]
    EmptyBundleId,
    /// A bundle must contain at least one component.
    #[error("bundle `{0}` must contain at least one component")]
    EmptyBundle(String),
    /// Bundle ids are unique within a profile.
    #[error("bundle `{0}` is declared more than once")]
    DuplicateBundle(String),
    /// A lifecycle operation failed or its outcome became uncertain.
    #[error("component `{component}` effect `{effect}` {phase} {kind}: {message}")]
    Lifecycle {
        component: String,
        effect: String,
        phase: LifecyclePhase,
        kind: LifecycleFailureKind,
        message: String,
    },
    /// Candidate activation failed and the previous composition could not be restored.
    #[error(
        "candidate activation failed ({candidate}); previous composition restore failed ({restore})"
    )]
    RestoreFailed {
        candidate: Box<RuntimeError>,
        restore: Box<RuntimeError>,
    },
    /// Failed and uncertain runtimes reject further mutations.
    #[error("runtime scope is {0} and cannot accept composition changes")]
    NotOperational(LifecycleState),
    /// Parent recomposition cannot leave a child component bound to stale services.
    #[error("live child scope `{scope}` is {state}; stop it before recomposing its parent")]
    LiveChildScope {
        scope: String,
        state: LifecycleState,
    },
    /// A closed runtime cannot accept more components.
    #[error("runtime scope is closed")]
    Closed,
}

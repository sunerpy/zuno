//! Scoped, transactional component assembly for Zuno harnesses.
//!
//! The runtime is independent of the agent loop. Components contribute typed
//! services and cleanup effects; profiles decide which components to mount.

use async_trait::async_trait;
use futures::future::BoxFuture;
use std::any::{Any, TypeId, type_name};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex as StdMutex, Weak};
use tokio::sync::Mutex as AsyncMutex;

type ErasedService = Arc<dyn Any + Send + Sync>;
type Cleanup = Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>;

/// A component that contributes services and effects to one harness scope.
#[async_trait]
pub trait Component: Send + Sync {
    /// Stable identity used for replacement and diagnostics.
    fn id(&self) -> &str;

    /// Stage this component's services and effects.
    ///
    /// Nothing becomes visible until this method returns successfully.
    async fn mount(&self, context: &mut MountContext) -> Result<(), RuntimeError>;
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

    /// Append one bundle in mount order.
    #[must_use]
    pub fn with_bundle(mut self, bundle: ProfileBundle) -> Self {
        self.bundles.push(bundle);
        self
    }
}

#[derive(Clone)]
struct StagedService {
    key: TypeId,
    value: ErasedService,
}

struct ServiceEntry {
    owner: String,
    value: ErasedService,
}

#[derive(Default)]
struct ScopeState {
    services: HashMap<TypeId, Vec<ServiceEntry>>,
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
        let local = {
            let state = self.inner.state.lock().expect("scope state poisoned");
            state
                .services
                .get(&TypeId::of::<T>())
                .and_then(|entries| entries.last())
                .map(|entry| Arc::clone(&entry.value))
        };

        if let Some(value) = local {
            return decode_service::<T>(&value);
        }

        self.inner.parent.as_ref().and_then(|parent| {
            Scope {
                inner: Arc::clone(parent),
            }
            .service::<T>()
        })
    }

    fn service_excluding_local<T>(&self, hidden_owners: &HashSet<String>) -> Option<Arc<T>>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        let local = {
            let state = self.inner.state.lock().expect("scope state poisoned");
            state.services.get(&TypeId::of::<T>()).and_then(|entries| {
                entries
                    .iter()
                    .rev()
                    .find(|entry| !hidden_owners.contains(&entry.owner))
                    .map(|entry| Arc::clone(&entry.value))
            })
        };

        if let Some(value) = local {
            return decode_service::<T>(&value);
        }

        self.inner.parent.as_ref().and_then(|parent| {
            Scope {
                inner: Arc::clone(parent),
            }
            .service::<T>()
        })
    }

    fn install(&self, owner: &str, services: Vec<StagedService>) {
        let mut state = self.inner.state.lock().expect("scope state poisoned");
        for service in services {
            state
                .services
                .entry(service.key)
                .or_default()
                .push(ServiceEntry {
                    owner: owner.to_owned(),
                    value: service.value,
                });
        }
    }

    fn replace(&self, owner: &str, services: Vec<StagedService>) {
        let mut state = self.inner.state.lock().expect("scope state poisoned");
        remove_owner(&mut state.services, owner);
        for service in services {
            state
                .services
                .entry(service.key)
                .or_default()
                .push(ServiceEntry {
                    owner: owner.to_owned(),
                    value: service.value,
                });
        }
    }

    fn replace_profile(
        &self,
        previous_owners: &HashSet<String>,
        components: Vec<(String, Vec<StagedService>)>,
    ) {
        let mut state = self.inner.state.lock().expect("scope state poisoned");
        let mut candidate = HashMap::<TypeId, Vec<ServiceEntry>>::new();
        for (owner, services) in components {
            for service in services {
                candidate
                    .entry(service.key)
                    .or_default()
                    .push(ServiceEntry {
                        owner: owner.clone(),
                        value: service.value,
                    });
            }
        }

        let keys = state
            .services
            .keys()
            .copied()
            .chain(candidate.keys().copied())
            .collect::<HashSet<_>>();
        for key in keys {
            let entries = state.services.entry(key).or_default();
            let insertion = entries
                .iter()
                .position(|entry| previous_owners.contains(&entry.owner))
                .unwrap_or(0);
            entries.retain(|entry| !previous_owners.contains(&entry.owner));
            if let Some(replacements) = candidate.remove(&key) {
                let insertion = insertion.min(entries.len());
                entries.splice(insertion..insertion, replacements);
            }
        }
        state.services.retain(|_, entries| !entries.is_empty());
    }

    fn remove(&self, owner: &str) {
        let mut state = self.inner.state.lock().expect("scope state poisoned");
        remove_owner(&mut state.services, owner);
    }

    fn clear(&self) {
        self.inner
            .state
            .lock()
            .expect("scope state poisoned")
            .services
            .clear();
    }
}

fn remove_owner(services: &mut HashMap<TypeId, Vec<ServiceEntry>>, owner: &str) {
    services.retain(|_, entries| {
        entries.retain(|entry| entry.owner != owner);
        !entries.is_empty()
    });
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

/// The staging context passed to one component mount.
pub struct MountContext {
    scope: Scope,
    candidate_services: Vec<StagedService>,
    hidden_owners: Arc<HashSet<String>>,
    services: Vec<StagedService>,
    cleanups: Vec<Cleanup>,
}

impl MountContext {
    fn new(scope: Scope) -> Self {
        Self::with_candidate(scope, Vec::new(), Arc::new(HashSet::new()))
    }

    fn with_candidate(
        scope: Scope,
        candidate_services: Vec<StagedService>,
        hidden_owners: Arc<HashSet<String>>,
    ) -> Self {
        Self {
            scope,
            candidate_services,
            hidden_owners,
            services: Vec::new(),
            cleanups: Vec::new(),
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
            value: erase_service(service),
        });
        Ok(())
    }

    /// Resolve one staged or already-mounted service.
    #[must_use]
    pub fn service<T>(&self) -> Option<Arc<T>>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        self.services
            .iter()
            .rev()
            .find(|service| service.key == TypeId::of::<T>())
            .and_then(|service| decode_service::<T>(&service.value))
            .or_else(|| {
                self.candidate_services
                    .iter()
                    .rev()
                    .find(|service| service.key == TypeId::of::<T>())
                    .and_then(|service| decode_service::<T>(&service.value))
            })
            .or_else(|| self.scope.service_excluding_local::<T>(&self.hidden_owners))
    }

    /// Resolve a required service or return a typed load-time error.
    pub fn require<T>(&self) -> Result<Arc<T>, RuntimeError>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        self.service::<T>()
            .ok_or(RuntimeError::MissingService(type_name::<T>()))
    }

    /// Register cleanup that runs on rollback, replacement, or shutdown.
    pub fn on_close<F, Fut>(&mut self, cleanup: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.cleanups.push(Box::new(move || Box::pin(cleanup())));
    }

    fn into_parts(self) -> (Vec<StagedService>, Vec<Cleanup>) {
        (self.services, self.cleanups)
    }

    async fn rollback(self) {
        run_cleanups(self.cleanups).await;
    }
}

struct MountedComponent {
    id: String,
    cleanups: Vec<Cleanup>,
}

struct MountedProfile {
    id: String,
    component_ids: Vec<String>,
    components: Vec<MountedComponent>,
}

impl MountedProfile {
    fn owner_ids(&self) -> HashSet<String> {
        self.component_ids.iter().cloned().collect()
    }

    fn owns(&self, component_id: &str) -> bool {
        self.component_ids
            .iter()
            .any(|candidate| candidate == component_id)
    }
}

#[derive(Default)]
struct RuntimeState {
    closed: bool,
    profile: Option<MountedProfile>,
    mounts: Vec<MountedComponent>,
    children: Vec<Weak<HarnessRuntimeInner>>,
}

struct HarnessRuntimeInner {
    scope: Scope,
    operation: Arc<AsyncMutex<()>>,
    state: StdMutex<RuntimeState>,
}

/// One independently configurable harness runtime.
#[derive(Clone)]
pub struct HarnessRuntime {
    inner: Arc<HarnessRuntimeInner>,
}

impl HarnessRuntime {
    /// Create one empty root runtime.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(HarnessRuntimeInner {
                scope: Scope::root(name.into()),
                operation: Arc::new(AsyncMutex::new(())),
                state: StdMutex::new(RuntimeState::default()),
            }),
        }
    }

    /// Return this runtime scope's diagnostic name.
    #[must_use]
    pub fn name(&self) -> &str {
        self.inner.scope.name()
    }

    /// Create an isolated child scope that inherits parent services.
    ///
    /// Shutting down the parent also shuts down all live children in reverse
    /// creation order.
    #[must_use]
    pub fn child(&self, name: impl Into<String>) -> Self {
        let scope = self.inner.scope.child(name.into());
        let mut parent = self.inner.state.lock().expect("runtime state poisoned");
        let closed = parent.closed;
        let child = Arc::new(HarnessRuntimeInner {
            scope,
            operation: Arc::clone(&self.inner.operation),
            state: StdMutex::new(RuntimeState {
                closed,
                ..RuntimeState::default()
            }),
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

    /// Return the active profile identifier.
    #[must_use]
    pub fn active_profile_id(&self) -> Option<String> {
        self.inner
            .state
            .lock()
            .expect("runtime state poisoned")
            .profile
            .as_ref()
            .map(|profile| profile.id.clone())
    }

    /// Stage and atomically activate one complete harness profile.
    ///
    /// Components mount in bundle order and may require services staged by earlier
    /// components. The previous profile remains visible until every candidate
    /// component succeeds.
    pub async fn activate_profile(&self, profile: HarnessProfile) -> Result<(), RuntimeError> {
        let _operation = self.inner.operation.lock().await;
        let profile = validate_profile(profile)?;
        let previous_owners = {
            let state = self.inner.state.lock().expect("runtime state poisoned");
            if state.closed {
                return Err(RuntimeError::Closed);
            }
            if state.profile.is_none() && !state.mounts.is_empty() {
                return Err(RuntimeError::ProfileAfterComponents);
            }
            for component_id in &profile.component_ids {
                if state
                    .mounts
                    .iter()
                    .any(|mounted| &mounted.id == component_id)
                {
                    return Err(RuntimeError::DuplicateComponent(component_id.clone()));
                }
            }
            state
                .profile
                .as_ref()
                .map_or_else(HashSet::new, MountedProfile::owner_ids)
        };

        let hidden_owners = Arc::new(previous_owners.clone());
        let mut candidate_services = Vec::new();
        let mut component_services = Vec::new();
        let mut mounted_components = Vec::new();
        for (component_id, component) in profile.components {
            let mut context = MountContext::with_candidate(
                self.inner.scope.clone(),
                candidate_services.clone(),
                Arc::clone(&hidden_owners),
            );
            if let Err(error) = component.mount(&mut context).await {
                context.rollback().await;
                cleanup_components(mounted_components).await;
                return Err(error);
            }
            let (services, cleanups) = context.into_parts();
            candidate_services.extend(services.iter().cloned());
            component_services.push((component_id.clone(), services));
            mounted_components.push(MountedComponent {
                id: component_id,
                cleanups,
            });
        }

        self.inner
            .scope
            .replace_profile(&previous_owners, component_services);
        let component_ids = mounted_components
            .iter()
            .map(|component| component.id.clone())
            .collect();
        let previous = self
            .inner
            .state
            .lock()
            .expect("runtime state poisoned")
            .profile
            .replace(MountedProfile {
                id: profile.id,
                component_ids,
                components: mounted_components,
            });
        if let Some(previous) = previous {
            cleanup_components(previous.components).await;
        }
        Ok(())
    }

    /// Mount a new component transactionally.
    pub async fn mount<C>(&self, component: C) -> Result<(), RuntimeError>
    where
        C: Component + 'static,
    {
        self.mount_shared(Arc::new(component)).await
    }

    /// Mount a dynamically selected component transactionally.
    pub async fn mount_shared(&self, component: Arc<dyn Component>) -> Result<(), RuntimeError> {
        let _operation = self.inner.operation.lock().await;
        self.mount_locked(component).await
    }

    async fn mount_locked(&self, component: Arc<dyn Component>) -> Result<(), RuntimeError> {
        let id = validate_component_id(component.id())?;
        {
            let state = self.inner.state.lock().expect("runtime state poisoned");
            if state.closed {
                return Err(RuntimeError::Closed);
            }
            if state.mounts.iter().any(|mounted| mounted.id == id)
                || state
                    .profile
                    .as_ref()
                    .is_some_and(|profile| profile.owns(&id))
            {
                return Err(RuntimeError::DuplicateComponent(id));
            }
        }

        let mut context = MountContext::new(self.inner.scope.clone());
        if let Err(error) = component.mount(&mut context).await {
            context.rollback().await;
            return Err(error);
        }
        let (services, cleanups) = context.into_parts();
        self.inner.scope.install(&id, services);
        self.inner
            .state
            .lock()
            .expect("runtime state poisoned")
            .mounts
            .push(MountedComponent { id, cleanups });
        Ok(())
    }

    /// Replace an existing component without exposing a partial candidate.
    pub async fn replace<C>(&self, component: C) -> Result<(), RuntimeError>
    where
        C: Component + 'static,
    {
        self.replace_shared(Arc::new(component)).await
    }

    /// Replace an existing dynamically selected component.
    pub async fn replace_shared(&self, component: Arc<dyn Component>) -> Result<(), RuntimeError> {
        let _operation = self.inner.operation.lock().await;
        let id = validate_component_id(component.id())?;
        let position = {
            let state = self.inner.state.lock().expect("runtime state poisoned");
            if state.closed {
                return Err(RuntimeError::Closed);
            }
            state
                .mounts
                .iter()
                .position(|mounted| mounted.id == id)
                .ok_or_else(|| RuntimeError::ComponentNotMounted(id.clone()))?
        };

        let mut context = MountContext::new(self.inner.scope.clone());
        if let Err(error) = component.mount(&mut context).await {
            context.rollback().await;
            return Err(error);
        }
        let (services, cleanups) = context.into_parts();
        self.inner.scope.replace(&id, services);
        let previous = {
            let mut state = self.inner.state.lock().expect("runtime state poisoned");
            std::mem::replace(
                &mut state.mounts[position],
                MountedComponent { id, cleanups },
            )
        };
        run_cleanups(previous.cleanups).await;
        Ok(())
    }

    /// Unmount one component and reveal any service it shadowed.
    pub async fn unmount(&self, id: &str) -> Result<(), RuntimeError> {
        let _operation = self.inner.operation.lock().await;
        let mounted = {
            let mut state = self.inner.state.lock().expect("runtime state poisoned");
            if state.closed {
                return Err(RuntimeError::Closed);
            }
            let position = state
                .mounts
                .iter()
                .position(|mounted| mounted.id == id)
                .ok_or_else(|| RuntimeError::ComponentNotMounted(id.to_owned()))?;
            state.mounts.remove(position)
        };
        self.inner.scope.remove(id);
        run_cleanups(mounted.cleanups).await;
        Ok(())
    }

    /// Close this runtime and its descendants.
    ///
    /// Children close in reverse creation order, then local components close
    /// in reverse mount order. Repeated shutdown calls are no-ops.
    pub async fn shutdown(&self) -> Result<(), RuntimeError> {
        let _operation = self.inner.operation.lock().await;
        shutdown_locked(Arc::clone(&self.inner)).await;
        Ok(())
    }
}

struct ValidatedProfile {
    id: String,
    component_ids: Vec<String>,
    components: Vec<(String, Arc<dyn Component>)>,
}

fn validate_profile(profile: HarnessProfile) -> Result<ValidatedProfile, RuntimeError> {
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
            components.push((component_id, component));
        }
    }
    Ok(ValidatedProfile {
        id: id.to_owned(),
        component_ids: component_ids.into_iter().collect(),
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

fn shutdown_locked(inner: Arc<HarnessRuntimeInner>) -> BoxFuture<'static, ()> {
    Box::pin(async move {
        let (profile, mounts, children) = {
            let mut state = inner.state.lock().expect("runtime state poisoned");
            if state.closed {
                return;
            }
            state.closed = true;
            let profile = state.profile.take();
            let mounts = std::mem::take(&mut state.mounts);
            let children = std::mem::take(&mut state.children);
            (profile, mounts, children)
        };
        inner.scope.clear();

        for child in children
            .into_iter()
            .rev()
            .filter_map(|child| child.upgrade())
        {
            shutdown_locked(child).await;
        }
        cleanup_components(mounts).await;
        if let Some(profile) = profile {
            cleanup_components(profile.components).await;
        }
    })
}

async fn cleanup_components(mut components: Vec<MountedComponent>) {
    while let Some(component) = components.pop() {
        run_cleanups(component.cleanups).await;
    }
}

async fn run_cleanups(mut cleanups: Vec<Cleanup>) {
    while let Some(cleanup) = cleanups.pop() {
        cleanup().await;
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
    /// A component id was mounted twice.
    #[error("component `{0}` is already mounted")]
    DuplicateComponent(String),
    /// Replacement or unmount named no mounted component.
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
    /// Profiles establish the foundation before local component overrides.
    #[error("a profile must be activated before local components are mounted")]
    ProfileAfterComponents,
    /// A closed runtime cannot accept more components.
    #[error("runtime scope is closed")]
    Closed,
}

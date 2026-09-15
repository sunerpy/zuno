use super::AgentBackendCapabilities;
use super::AgentBackendDescriptor;
use super::AgentBackendId;
use super::AgentBackendOperation;
use super::AgentBackendRegistryError;
use super::AgentBackendRequirements;
use crate::OneShotAgentBackend;
use crate::OneShotAgentBackendKind;
use crate::OneShotAgentError;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::sync::PoisonError;
use std::sync::RwLock;
use std::sync::Weak;

/// Creates one configured backend instance from host-owned context.
///
/// Construction must not start product work. The registry validates advertised
/// capabilities before calling `build`, then verifies that the returned backend
/// still matches the factory descriptor. This lets hosts resolve Profiles and
/// other trusted configuration without hard-coding workflow names or model
/// routes into the runtime.
pub trait AgentBackendFactory<C>: Send + Sync {
    fn kind(&self) -> OneShotAgentBackendKind;

    fn capabilities(&self) -> AgentBackendCapabilities;

    /// Stable revision of the implementation and declaration that constructs
    /// this backend. The value must change whenever a remounted factory with
    /// the same id could produce materially different behavior.
    fn revision(&self) -> String;

    fn build(&self, context: &C) -> Result<Arc<dyn OneShotAgentBackend>, OneShotAgentError>;
}

struct MountedFactory<C> {
    token: Arc<()>,
    descriptor: AgentBackendDescriptor,
    factory: Arc<dyn AgentBackendFactory<C>>,
}

struct AgentBackendFactoryRegistryInner<C> {
    mounted: RwLock<BTreeMap<AgentBackendId, MountedFactory<C>>>,
}

impl<C> Default for AgentBackendFactoryRegistryInner<C> {
    fn default() -> Self {
        Self {
            mounted: RwLock::new(BTreeMap::new()),
        }
    }
}

/// Concurrent registry of replaceable Agent backend factories.
///
/// A factory is selected by the workflow's `agentRef`; provider/model/permission
/// choices remain in the host-owned build context. Resolution retains one exact
/// registration generation, so plugin unmount prevents new construction without
/// invalidating a build that was already admitted.
pub struct AgentBackendFactoryRegistry<C> {
    inner: Arc<AgentBackendFactoryRegistryInner<C>>,
}

impl<C> Clone for AgentBackendFactoryRegistry<C> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<C> Default for AgentBackendFactoryRegistry<C> {
    fn default() -> Self {
        Self {
            inner: Arc::new(AgentBackendFactoryRegistryInner::default()),
        }
    }
}

impl<C: 'static> AgentBackendFactoryRegistry<C> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mount one factory and return the exact disposer for that generation.
    pub fn mount(
        &self,
        id: AgentBackendId,
        factory: Arc<dyn AgentBackendFactory<C>>,
    ) -> Result<AgentBackendFactoryMount<C>, AgentBackendRegistryError> {
        let capabilities = factory.capabilities();
        if !capabilities
            .operations
            .contains(&AgentBackendOperation::OneShot)
        {
            return Err(AgentBackendRegistryError::InvalidCapabilities { id });
        }
        let revision = factory.revision();
        if revision.is_empty()
            || revision.trim() != revision
            || revision.chars().count() > 256
            || revision.chars().any(char::is_control)
        {
            return Err(AgentBackendRegistryError::InvalidRevision { id });
        }
        let descriptor = AgentBackendDescriptor {
            id: id.clone(),
            kind: factory.kind(),
            capabilities,
            revision: Some(revision),
        };
        let token = Arc::new(());
        let mut mounted = self
            .inner
            .mounted
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if mounted.contains_key(&id) {
            return Err(AgentBackendRegistryError::Duplicate { id });
        }
        mounted.insert(
            id.clone(),
            MountedFactory {
                token: Arc::clone(&token),
                descriptor,
                factory,
            },
        );
        Ok(AgentBackendFactoryMount {
            registry: Arc::downgrade(&self.inner),
            id,
            token,
            disposed: false,
        })
    }

    /// Return a stable, ID-sorted inventory without constructing a backend.
    pub fn list(&self) -> Vec<AgentBackendDescriptor> {
        self.inner
            .mounted
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(|mounted| mounted.descriptor.clone())
            .collect()
    }

    /// Resolve one exact factory generation and validate capabilities first.
    pub fn resolve(
        &self,
        id: &AgentBackendId,
        requirements: &AgentBackendRequirements,
    ) -> Result<ResolvedAgentBackendFactory<C>, AgentBackendRegistryError> {
        let mounted = self
            .inner
            .mounted
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let mounted = mounted
            .get(id)
            .ok_or_else(|| AgentBackendRegistryError::NotFound { id: id.clone() })?;
        let missing = mounted.descriptor.capabilities.missing(requirements);
        if !missing.is_empty() {
            return Err(AgentBackendRegistryError::Unsupported {
                id: id.clone(),
                missing,
            });
        }
        Ok(ResolvedAgentBackendFactory {
            descriptor: mounted.descriptor.clone(),
            factory: Arc::clone(&mounted.factory),
        })
    }

    /// Resolve and construct a backend without starting its one-shot run.
    pub fn build(
        &self,
        id: &AgentBackendId,
        requirements: &AgentBackendRequirements,
        context: &C,
    ) -> Result<Arc<dyn OneShotAgentBackend>, AgentBackendFactoryError> {
        self.resolve(id, requirements)
            .map_err(AgentBackendFactoryError::Registry)?
            .build(context)
    }
}

/// One resolved factory generation that remains valid after its mount is removed.
pub struct ResolvedAgentBackendFactory<C> {
    descriptor: AgentBackendDescriptor,
    factory: Arc<dyn AgentBackendFactory<C>>,
}

impl<C> Clone for ResolvedAgentBackendFactory<C> {
    fn clone(&self) -> Self {
        Self {
            descriptor: self.descriptor.clone(),
            factory: Arc::clone(&self.factory),
        }
    }
}

impl<C> fmt::Debug for ResolvedAgentBackendFactory<C> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedAgentBackendFactory")
            .field("descriptor", &self.descriptor)
            .finish_non_exhaustive()
    }
}

impl<C> ResolvedAgentBackendFactory<C> {
    pub fn descriptor(&self) -> &AgentBackendDescriptor {
        &self.descriptor
    }

    pub fn build(
        &self,
        context: &C,
    ) -> Result<Arc<dyn OneShotAgentBackend>, AgentBackendFactoryError> {
        let backend = self
            .factory
            .build(context)
            .map_err(AgentBackendFactoryError::Backend)?;
        let actual_kind = backend.kind();
        let actual_capabilities = backend.capabilities();
        if actual_kind != self.descriptor.kind
            || actual_capabilities != self.descriptor.capabilities
        {
            return Err(AgentBackendFactoryError::ContractMismatch {
                id: self.descriptor.id.clone(),
                expected_kind: self.descriptor.kind,
                actual_kind,
                expected_capabilities: self.descriptor.capabilities.clone(),
                actual_capabilities,
            });
        }
        Ok(backend)
    }
}

/// Disposer for exactly one mounted factory generation.
#[must_use = "dropping the mount immediately withdraws the Agent backend factory"]
pub struct AgentBackendFactoryMount<C> {
    registry: Weak<AgentBackendFactoryRegistryInner<C>>,
    id: AgentBackendId,
    token: Arc<()>,
    disposed: bool,
}

impl<C> fmt::Debug for AgentBackendFactoryMount<C> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentBackendFactoryMount")
            .field("id", &self.id)
            .field("disposed", &self.disposed)
            .finish_non_exhaustive()
    }
}

impl<C> AgentBackendFactoryMount<C> {
    pub fn dispose(mut self) -> bool {
        self.dispose_inner()
    }

    fn dispose_inner(&mut self) -> bool {
        if self.disposed {
            return false;
        }
        self.disposed = true;
        let Some(registry) = self.registry.upgrade() else {
            return false;
        };
        let mut mounted = registry
            .mounted
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let is_current = mounted
            .get(&self.id)
            .is_some_and(|entry| Arc::ptr_eq(&entry.token, &self.token));
        if !is_current {
            return false;
        }
        mounted.remove(&self.id);
        true
    }
}

impl<C> Drop for AgentBackendFactoryMount<C> {
    fn drop(&mut self) {
        self.dispose_inner();
    }
}

/// Factory resolution or construction failure.
#[derive(Debug, Eq, PartialEq)]
pub enum AgentBackendFactoryError {
    Registry(AgentBackendRegistryError),
    Backend(OneShotAgentError),
    ContractMismatch {
        id: AgentBackendId,
        expected_kind: OneShotAgentBackendKind,
        actual_kind: OneShotAgentBackendKind,
        expected_capabilities: AgentBackendCapabilities,
        actual_capabilities: AgentBackendCapabilities,
    },
}

impl fmt::Display for AgentBackendFactoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registry(error) => error.fmt(formatter),
            Self::Backend(error) => error.fmt(formatter),
            Self::ContractMismatch {
                id,
                expected_kind,
                actual_kind,
                ..
            } => write!(
                formatter,
                "Agent backend factory `{id}` returned `{actual_kind}` instead of advertised `{expected_kind}` capabilities"
            ),
        }
    }
}

impl std::error::Error for AgentBackendFactoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Registry(error) => Some(error),
            Self::Backend(error) => Some(error),
            Self::ContractMismatch { .. } => None,
        }
    }
}

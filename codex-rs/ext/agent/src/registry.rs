mod factory;
mod types;

pub use factory::*;
pub use types::*;

use crate::OneShotAgentBackend;
use crate::OneShotAgentError;
use crate::OneShotAgentRequest;
use crate::OneShotAgentResult;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::sync::PoisonError;
use std::sync::RwLock;
use std::sync::Weak;
use tokio_util::sync::CancellationToken;

struct MountedBackend {
    token: Arc<()>,
    descriptor: AgentBackendDescriptor,
    backend: Arc<dyn OneShotAgentBackend>,
}

#[derive(Default)]
struct AgentBackendRegistryInner {
    mounted: RwLock<BTreeMap<AgentBackendId, MountedBackend>>,
}

/// Concurrent registry of configured Agent backend instances.
///
/// Resolution clones one exact mounted generation before dispatch, so unloading
/// a plugin prevents new work without invalidating an already admitted run.
#[derive(Clone, Default)]
pub struct AgentBackendRegistry {
    inner: Arc<AgentBackendRegistryInner>,
}

impl AgentBackendRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mount one backend and return the exact disposer for that registration.
    pub fn mount(
        &self,
        id: AgentBackendId,
        backend: Arc<dyn OneShotAgentBackend>,
    ) -> Result<AgentBackendMount, AgentBackendRegistryError> {
        let capabilities = backend.capabilities();
        if !capabilities
            .operations
            .contains(&AgentBackendOperation::OneShot)
        {
            return Err(AgentBackendRegistryError::InvalidCapabilities { id });
        }
        let descriptor = AgentBackendDescriptor {
            id: id.clone(),
            kind: backend.kind(),
            capabilities,
            revision: None,
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
            MountedBackend {
                token: Arc::clone(&token),
                descriptor,
                backend,
            },
        );
        Ok(AgentBackendMount {
            registry: Arc::downgrade(&self.inner),
            id,
            token,
            disposed: false,
        })
    }

    /// Return a stable, ID-sorted inventory snapshot.
    pub fn list(&self) -> Vec<AgentBackendDescriptor> {
        self.inner
            .mounted
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(|mounted| mounted.descriptor.clone())
            .collect()
    }

    /// Resolve and validate one exact backend generation without starting work.
    pub fn resolve(
        &self,
        id: &AgentBackendId,
        requirements: &AgentBackendRequirements,
    ) -> Result<ResolvedAgentBackend, AgentBackendRegistryError> {
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
        Ok(ResolvedAgentBackend {
            descriptor: mounted.descriptor.clone(),
            backend: Arc::clone(&mounted.backend),
        })
    }

    /// Resolve capabilities first, then run one admitted invocation.
    pub async fn dispatch(
        &self,
        id: &AgentBackendId,
        requirements: &AgentBackendRequirements,
        request: OneShotAgentRequest,
        cancellation: CancellationToken,
    ) -> Result<OneShotAgentResult, AgentBackendDispatchError> {
        let resolved = self
            .resolve(id, requirements)
            .map_err(AgentBackendDispatchError::Registry)?;
        resolved
            .run(request, cancellation)
            .await
            .map_err(AgentBackendDispatchError::Backend)
    }
}

/// One resolved generation safe to retain for an already admitted dispatch.
#[derive(Clone)]
pub struct ResolvedAgentBackend {
    descriptor: AgentBackendDescriptor,
    backend: Arc<dyn OneShotAgentBackend>,
}

impl fmt::Debug for ResolvedAgentBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedAgentBackend")
            .field("descriptor", &self.descriptor)
            .finish_non_exhaustive()
    }
}

impl ResolvedAgentBackend {
    pub fn descriptor(&self) -> &AgentBackendDescriptor {
        &self.descriptor
    }

    pub async fn run(
        &self,
        request: OneShotAgentRequest,
        cancellation: CancellationToken,
    ) -> Result<OneShotAgentResult, OneShotAgentError> {
        self.backend.run(request, cancellation).await
    }
}

/// Disposer for exactly one mounted backend generation.
///
/// Dropping the disposer withdraws the backend from future resolution. Runs
/// holding a [`ResolvedAgentBackend`] continue against their admitted snapshot.
#[must_use = "dropping the mount immediately withdraws the Agent backend"]
pub struct AgentBackendMount {
    registry: Weak<AgentBackendRegistryInner>,
    id: AgentBackendId,
    token: Arc<()>,
    disposed: bool,
}

impl fmt::Debug for AgentBackendMount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentBackendMount")
            .field("id", &self.id)
            .field("disposed", &self.disposed)
            .finish_non_exhaustive()
    }
}

impl AgentBackendMount {
    /// Withdraw this registration. Returns whether this exact generation existed.
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

impl Drop for AgentBackendMount {
    fn drop(&mut self) {
        self.dispose_inner();
    }
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "registry/factory_tests.rs"]
mod factory_tests;

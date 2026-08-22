use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

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

/// Process-local lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DynamicState {
    Defined,
    Running,
    Stopped,
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
}

#[derive(Debug, Clone)]
struct DynamicPackage {
    package: Package,
    state: DynamicState,
}

/// In-memory definitions owned by one Zuno process.
///
/// Nothing in this type serializes to disk. Replacing it with a fresh registry is
/// the process-restart boundary and intentionally loses every definition.
#[derive(Debug, Default)]
pub struct ExtensionRegistry {
    definitions: RwLock<BTreeMap<Scope, Vec<DynamicPackage>>>,
    composition_generation: AtomicU64,
}

impl ExtensionRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Revision of the active composition only.
    ///
    /// Defining an inactive package does not force a host rebuild. Running,
    /// stopping, or removing a running package does.
    #[must_use]
    pub fn composition_generation(&self) -> u64 {
        self.composition_generation.load(Ordering::Acquire)
    }

    pub fn define(&self, scope: &Scope, package: Package) -> Result<(), RegistryError> {
        package.validate().map_err(RegistryError::Manifest)?;
        let mut definitions = self.write();
        let scoped = definitions.entry(scope.clone()).or_default();
        if scoped.iter().any(|entry| entry.package.id == package.id) {
            return Err(RegistryError::AlreadyDefined(package.id));
        }
        scoped.push(DynamicPackage {
            package,
            state: DynamicState::Defined,
        });
        Ok(())
    }

    pub fn run(&self, scope: &Scope, id: &str) -> Result<(), RegistryError> {
        self.run_with_static(scope, id, &[])
    }

    /// Activate transactionally against the static and already-running packages.
    pub fn run_with_static(
        &self,
        scope: &Scope,
        id: &str,
        static_packages: &[StaticPackage],
    ) -> Result<(), RegistryError> {
        let mut definitions = self.write();
        let scoped = definitions
            .get_mut(scope)
            .ok_or_else(|| RegistryError::NotFound(id.to_owned()))?;
        let Some(target) = scoped.iter().position(|entry| entry.package.id == id) else {
            return Err(RegistryError::NotFound(id.to_owned()));
        };
        if scoped[target].state == DynamicState::Running {
            return Ok(());
        }
        let candidate = scoped
            .iter()
            .enumerate()
            .filter(|(index, entry)| *index == target || entry.state == DynamicState::Running)
            .map(|(_, entry)| entry.package.clone())
            .collect::<Vec<_>>();
        resolve_active_packages(static_packages, candidate)
            .map_err(|error| RegistryError::Activation(Box::new(error)))?;
        scoped[target].state = DynamicState::Running;
        drop(definitions);
        self.composition_generation.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    pub fn stop(&self, scope: &Scope, id: &str) -> Result<(), RegistryError> {
        let changed = {
            let mut definitions = self.write();
            let entry = find_mut(&mut definitions, scope, id)?;
            let changed = entry.state == DynamicState::Running;
            entry.state = DynamicState::Stopped;
            changed
        };
        if changed {
            self.composition_generation.fetch_add(1, Ordering::AcqRel);
        }
        Ok(())
    }

    pub fn undefine(&self, scope: &Scope, id: &str) -> Result<(), RegistryError> {
        let was_running = {
            let mut definitions = self.write();
            let Some(scoped) = definitions.get_mut(scope) else {
                return Err(RegistryError::NotFound(id.to_owned()));
            };
            let Some(index) = scoped.iter().position(|entry| entry.package.id == id) else {
                return Err(RegistryError::NotFound(id.to_owned()));
            };
            let removed = scoped.remove(index);
            if scoped.is_empty() {
                definitions.remove(scope);
            }
            removed.state == DynamicState::Running
        };
        if was_running {
            self.composition_generation.fetch_add(1, Ordering::AcqRel);
        }
        Ok(())
    }

    #[must_use]
    pub fn dynamic_statuses(&self, scope: &Scope) -> Vec<PackageStatus> {
        self.read()
            .get(scope)
            .into_iter()
            .flatten()
            .map(|entry| status(&entry.package, entry.state))
            .collect()
    }

    pub(crate) fn running(&self, scope: &Scope) -> Vec<Package> {
        self.read()
            .get(scope)
            .into_iter()
            .flatten()
            .filter(|entry| entry.state == DynamicState::Running)
            .map(|entry| entry.package.clone())
            .collect()
    }

    fn read(&self) -> RwLockReadGuard<'_, BTreeMap<Scope, Vec<DynamicPackage>>> {
        self.definitions
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, BTreeMap<Scope, Vec<DynamicPackage>>> {
        self.definitions
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn find_mut<'a>(
    definitions: &'a mut BTreeMap<Scope, Vec<DynamicPackage>>,
    scope: &Scope,
    id: &str,
) -> Result<&'a mut DynamicPackage, RegistryError> {
    definitions
        .get_mut(scope)
        .and_then(|packages| packages.iter_mut().find(|entry| entry.package.id == id))
        .ok_or_else(|| RegistryError::NotFound(id.to_owned()))
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
    #[error("extension package activation failed")]
    Activation(#[source] Box<crate::resolve::ResolveError>),
}

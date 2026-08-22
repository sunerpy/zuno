use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::Serialize;
use zuno_config::schema::CommandConfig;
use zuno_config::schema::agent::AgentConfig;
use zuno_config::schema::ordered::OrderedMap;

use crate::{ExtensionRegistry, Package, Scope, StaticPackage};

/// Where an active package came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "lifetime", rename_all = "snake_case")]
pub enum PackageOrigin {
    Static { manifest: PathBuf },
    Process,
}

/// One active package with provenance.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResolvedPackage {
    pub package: Package,
    pub origin: PackageOrigin,
}

/// The validated contribution view consumed by agents, commands, skills, and
/// prompt assembly.
#[derive(Debug, Clone, Default)]
pub struct ResolvedExtensions {
    packages: Vec<ResolvedPackage>,
    agents: OrderedMap<AgentConfig>,
    workflows: OrderedMap<CommandConfig>,
    skills: Vec<zuno_catalog::skill::Skill>,
}

impl ResolvedExtensions {
    #[must_use]
    pub fn packages(&self) -> &[ResolvedPackage] {
        &self.packages
    }

    #[must_use]
    pub fn agents(&self) -> &OrderedMap<AgentConfig> {
        &self.agents
    }

    #[must_use]
    pub fn workflows(&self) -> &OrderedMap<CommandConfig> {
        &self.workflows
    }

    #[must_use]
    pub fn skills(&self) -> &[zuno_catalog::skill::Skill] {
        &self.skills
    }

    /// Stable model-facing explanation plus the exact active package projection.
    #[must_use]
    pub fn prompt_section(&self) -> String {
        let mut output = String::from(
            "Zuno supports validated extension packages. Use `extension_define`, \
             `extension_run`, `extension_stop`, `extension_undefine`, and \
             `extension_inspect` for process-local packages. Process-local definitions \
             never write disk and disappear when Zuno exits. For restart-persistent \
             loading, write `.zuno/extensions/<id>/extension.json` and restart Zuno. \
             Use the `customize-zuno` skill for the manifest schema.",
        );
        if self.packages.is_empty() {
            output.push_str("\n\nActive extension packages: none.");
            return output;
        }
        output.push_str("\n\nActive extension packages:");
        for resolved in &self.packages {
            let source = match &resolved.origin {
                PackageOrigin::Static { manifest } => manifest.display().to_string(),
                PackageOrigin::Process => "current process".to_owned(),
            };
            output.push_str(&format!(
                "\n- `{}` from {}: agents [{}]; workflows [{}]; skills [{}]",
                resolved.package.id,
                source,
                joined(resolved.package.agents.keys()),
                joined(resolved.package.workflows.keys()),
                joined(
                    resolved
                        .package
                        .skills
                        .iter()
                        .map(|skill| skill.name.as_str())
                ),
            ));
        }
        output
    }
}

fn joined<'a>(items: impl Iterator<Item = &'a str>) -> String {
    let values = items.collect::<Vec<_>>();
    if values.is_empty() {
        "none".to_owned()
    } else {
        values.join(", ")
    }
}

/// Merge static and running process packages into one collision-checked view.
pub fn resolve_active(
    scope: &Scope,
    static_packages: &[StaticPackage],
    registry: &ExtensionRegistry,
) -> Result<ResolvedExtensions, ResolveError> {
    resolve_active_packages(static_packages, registry.running(scope))
}

/// Resolve the prepared composition without publishing it as active.
pub fn resolve_desired(
    scope: &Scope,
    static_packages: &[StaticPackage],
    registry: &ExtensionRegistry,
) -> Result<ResolvedExtensions, ResolveError> {
    resolve_active_packages(static_packages, registry.desired(scope))
}

pub(crate) fn resolve_active_packages(
    static_packages: &[StaticPackage],
    dynamic_packages: impl IntoIterator<Item = Package>,
) -> Result<ResolvedExtensions, ResolveError> {
    let mut packages = static_packages
        .iter()
        .map(|entry| ResolvedPackage {
            package: entry.package().clone(),
            origin: PackageOrigin::Static {
                manifest: entry.manifest().to_path_buf(),
            },
        })
        .collect::<Vec<_>>();
    packages.extend(dynamic_packages.into_iter().map(|package| ResolvedPackage {
        package,
        origin: PackageOrigin::Process,
    }));

    let mut package_ids = BTreeSet::new();
    let mut owners: BTreeMap<(&'static str, String), String> = BTreeMap::new();
    let mut agents = OrderedMap::new();
    let mut workflows = OrderedMap::new();
    let mut skills = Vec::new();
    for resolved in &packages {
        let package = &resolved.package;
        package.validate().map_err(ResolveError::Manifest)?;
        if !package_ids.insert(package.id.clone()) {
            return Err(ResolveError::DuplicatePackage(package.id.clone()));
        }
        for (name, agent) in package.agents.iter() {
            claim(&mut owners, "agent", name, &package.id)?;
            agents.insert(name, agent.clone());
        }
        for (name, workflow) in package.workflows.iter() {
            claim(&mut owners, "workflow", name, &package.id)?;
            workflows.insert(
                name,
                CommandConfig {
                    template: workflow.prompt.clone(),
                    description: workflow.description.clone(),
                    agent: None,
                    model: None,
                    variant: None,
                    subtask: None,
                },
            );
        }
        for skill in &package.skills {
            claim(&mut owners, "skill", &skill.name, &package.id)?;
            let location = match &resolved.origin {
                PackageOrigin::Static { manifest } => manifest.display().to_string(),
                PackageOrigin::Process => format!("<process-extension:{}>", package.id),
            };
            skills.push(zuno_catalog::skill::Skill {
                name: skill.name.clone(),
                description: Some(skill.description.clone()),
                location,
                content: skill.content.clone(),
            });
        }
    }

    Ok(ResolvedExtensions {
        packages,
        agents,
        workflows,
        skills,
    })
}

fn claim(
    owners: &mut BTreeMap<(&'static str, String), String>,
    kind: &'static str,
    name: &str,
    package: &str,
) -> Result<(), ResolveError> {
    let key = (kind, name.to_owned());
    if let Some(existing) = owners.insert(key, package.to_owned()) {
        return Err(ResolveError::DuplicateContribution {
            kind,
            name: name.to_owned(),
            first: existing,
            second: package.to_owned(),
        });
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error(transparent)]
    Manifest(#[from] crate::manifest::ManifestError),
    #[error("extension package `{0}` is active more than once")]
    DuplicatePackage(String),
    #[error(
        "extension {kind} `{name}` is contributed by both package `{first}` and package `{second}`"
    )]
    DuplicateContribution {
        kind: &'static str,
        name: String,
        first: String,
        second: String,
    },
}

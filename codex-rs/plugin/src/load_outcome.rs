use std::collections::HashMap;
use std::collections::HashSet;

use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_plugins::PluginIdentity;
use codex_utils_plugins::PluginSkillRoot;
use codex_utils_plugins::SkillDiscoveryMode;

use crate::AppConnectorId;
use crate::AppDeclaration;
use crate::EffectivePluginAgentBackend;
use crate::PluginAgentBackendDeclaration;
use crate::PluginCapabilitySummary;
use crate::PluginHookSource;
use crate::app_connector_ids_from_declarations;

const MAX_CAPABILITY_SUMMARY_DESCRIPTION_LEN: usize = 1024;

/// A plugin that was loaded from disk, including merged MCP server definitions.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedPlugin<M> {
    pub config_name: String,
    pub remote_plugin_id: Option<String>,
    pub manifest_name: Option<String>,
    pub plugin_namespace: Option<String>,
    pub manifest_description: Option<String>,
    pub root: AbsolutePathBuf,
    pub enabled: bool,
    pub skill_roots: Vec<AbsolutePathBuf>,
    /// Workflow files or directories explicitly declared by this plugin.
    pub workflow_roots: Vec<AbsolutePathBuf>,
    /// Validated Agent backend factories explicitly declared by this plugin.
    pub agent_backends: Vec<PluginAgentBackendDeclaration>,
    pub skill_discovery_mode: SkillDiscoveryMode,
    pub disabled_skill_paths: HashSet<AbsolutePathBuf>,
    pub has_enabled_skills: bool,
    pub mcp_servers: HashMap<String, M>,
    pub apps: Vec<AppDeclaration>,
    pub hook_sources: Vec<PluginHookSource>,
    pub hook_load_warnings: Vec<String>,
    pub error: Option<String>,
}

impl<M> LoadedPlugin<M> {
    pub fn is_active(&self) -> bool {
        self.enabled && self.error.is_none()
    }

    pub fn display_name(&self) -> &str {
        self.manifest_name.as_deref().unwrap_or(&self.config_name)
    }

    pub fn is_agent_plugin(&self) -> bool {
        self.skill_discovery_mode == SkillDiscoveryMode::DirectChildren
    }
}

fn plugin_capability_summary_from_loaded<M>(
    plugin: &LoadedPlugin<M>,
) -> Option<PluginCapabilitySummary> {
    if !plugin.is_active() {
        return None;
    }

    let mut mcp_server_names: Vec<String> = plugin.mcp_servers.keys().cloned().collect();
    mcp_server_names.sort_unstable();

    let summary = PluginCapabilitySummary {
        config_name: plugin.config_name.clone(),
        display_name: plugin.display_name().to_string(),
        plugin_namespace: plugin.plugin_namespace.clone(),
        description: prompt_safe_plugin_description(plugin.manifest_description.as_deref()),
        has_skills: plugin.has_enabled_skills,
        mcp_server_names,
        app_connector_ids: app_connector_ids_from_declarations(&plugin.apps),
    };

    (summary.has_skills
        || !summary.mcp_server_names.is_empty()
        || !summary.app_connector_ids.is_empty())
    .then_some(summary)
}

/// Normalizes plugin descriptions for inclusion in model-facing capability summaries.
pub fn prompt_safe_plugin_description(description: Option<&str>) -> Option<String> {
    let description = description?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if description.is_empty() {
        return None;
    }

    Some(
        description
            .chars()
            .take(MAX_CAPABILITY_SUMMARY_DESCRIPTION_LEN)
            .collect(),
    )
}

/// Runtime view of loaded plugins and their derived capability summaries.
///
/// Runtime exclusions retain loaded metadata while removing derived capabilities.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginLoadOutcome<M> {
    plugins: Vec<LoadedPlugin<M>>,
    capability_summaries: Vec<PluginCapabilitySummary>,
}

impl<M: Clone> Default for PluginLoadOutcome<M> {
    fn default() -> Self {
        Self::from_plugins(Vec::new())
    }
}

impl<M: Clone> PluginLoadOutcome<M> {
    pub fn from_plugins(plugins: Vec<LoadedPlugin<M>>) -> Self {
        let capability_summaries = plugins
            .iter()
            .filter_map(plugin_capability_summary_from_loaded)
            .collect::<Vec<_>>();
        Self {
            plugins,
            capability_summaries,
        }
    }

    /// Marks matching canonical plugin IDs inactive while retaining their loaded metadata.
    pub fn without_plugins(mut self, disabled_plugin_ids: &[String]) -> Self {
        if disabled_plugin_ids.is_empty() {
            return self;
        }
        for plugin in &mut self.plugins {
            if disabled_plugin_ids.contains(&plugin.config_name) {
                plugin.enabled = false;
            }
        }
        Self::from_plugins(self.plugins)
    }

    pub fn effective_plugin_skill_roots(&self) -> Vec<PluginSkillRoot> {
        let mut skill_roots = Vec::new();
        let mut seen_paths = HashSet::new();
        for plugin in self.plugins.iter().filter(|plugin| plugin.is_active()) {
            let Some(plugin_namespace) = &plugin.plugin_namespace else {
                continue;
            };
            for path in &plugin.skill_roots {
                if seen_paths.insert(path.clone()) {
                    skill_roots.push(PluginSkillRoot {
                        path: path.clone(),
                        plugin_identity: PluginIdentity {
                            plugin_id: plugin.config_name.clone(),
                            remote_plugin_id: plugin.remote_plugin_id.clone(),
                        },
                        plugin_namespace: plugin_namespace.clone(),
                        plugin_root: plugin.root.clone(),
                        discovery_mode: plugin.skill_discovery_mode,
                    });
                }
            }
        }

        skill_roots.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        skill_roots
    }

    /// Returns active plugin workflow roots with their owning plugin identity.
    ///
    /// Paths retain plugin attribution so a workflow registry can apply plugin precedence
    /// without treating installed plugin content as an application builtin.
    pub fn effective_plugin_workflow_roots(&self) -> Vec<(AbsolutePathBuf, PluginIdentity)> {
        let mut roots = Vec::new();
        let mut seen_paths = HashSet::new();
        for plugin in self.plugins.iter().filter(|plugin| plugin.is_active()) {
            for path in &plugin.workflow_roots {
                if seen_paths.insert(path.clone()) {
                    roots.push((
                        path.clone(),
                        PluginIdentity {
                            plugin_id: plugin.config_name.clone(),
                            remote_plugin_id: plugin.remote_plugin_id.clone(),
                        },
                    ));
                }
            }
        }
        roots.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        roots
    }

    /// Returns every active plugin Agent backend with a namespaced runtime ID.
    ///
    /// Unlike path roots, declarations are not deduplicated: two active plugins
    /// claiming the same namespace remain visible so the factory registry can
    /// reject the conflict instead of silently selecting one implementation.
    pub fn effective_plugin_agent_backends(&self) -> Vec<EffectivePluginAgentBackend> {
        let mut backends = self
            .plugins
            .iter()
            .filter(|plugin| plugin.is_active())
            .flat_map(|plugin| {
                let namespace = plugin.plugin_namespace.as_deref()?;
                let plugin_identity = PluginIdentity {
                    plugin_id: plugin.config_name.clone(),
                    remote_plugin_id: plugin.remote_plugin_id.clone(),
                };
                Some(
                    plugin
                        .agent_backends
                        .iter()
                        .cloned()
                        .map(move |declaration| EffectivePluginAgentBackend {
                            id: format!("{namespace}/{}", declaration.local_id),
                            plugin_identity: plugin_identity.clone(),
                            declaration,
                        }),
                )
            })
            .flatten()
            .collect::<Vec<_>>();
        backends.sort_unstable_by(|left, right| {
            left.id.cmp(&right.id).then_with(|| {
                left.plugin_identity
                    .plugin_id
                    .cmp(&right.plugin_identity.plugin_id)
            })
        });
        backends
    }

    pub fn effective_mcp_servers(&self) -> HashMap<String, M> {
        let mut mcp_servers = HashMap::new();
        for plugin in self.plugins.iter().filter(|plugin| plugin.is_active()) {
            for (name, config) in &plugin.mcp_servers {
                mcp_servers
                    .entry(name.clone())
                    .or_insert_with(|| config.clone());
            }
        }
        mcp_servers
    }

    pub fn effective_apps(&self) -> Vec<AppConnectorId> {
        app_connector_ids_from_declarations(
            self.plugins
                .iter()
                .filter(|plugin| plugin.is_active())
                .flat_map(|plugin| plugin.apps.iter()),
        )
    }

    pub fn effective_plugin_hook_sources(&self) -> Vec<PluginHookSource> {
        self.iter_effective_plugin_hook_sources().cloned().collect()
    }

    pub fn iter_effective_plugin_hook_sources(&self) -> impl Iterator<Item = &PluginHookSource> {
        self.plugins
            .iter()
            .filter(|plugin| plugin.is_active())
            .flat_map(|plugin| plugin.hook_sources.iter())
    }

    pub fn effective_plugin_hook_warnings(&self) -> Vec<String> {
        self.iter_effective_plugin_hook_warnings()
            .cloned()
            .collect()
    }

    pub fn iter_effective_plugin_hook_warnings(&self) -> impl Iterator<Item = &String> {
        self.plugins
            .iter()
            .filter(|plugin| plugin.is_active())
            .flat_map(|plugin| plugin.hook_load_warnings.iter())
    }

    pub fn capability_summaries(&self) -> &[PluginCapabilitySummary] {
        &self.capability_summaries
    }

    pub fn plugins(&self) -> &[LoadedPlugin<M>] {
        &self.plugins
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path(name: &str) -> AbsolutePathBuf {
        AbsolutePathBuf::from_absolute_path_checked(std::env::temp_dir().join(name))
            .expect("absolute temp path")
    }

    fn loaded_plugin(config_name: &str, skill_roots: Vec<AbsolutePathBuf>) -> LoadedPlugin<()> {
        LoadedPlugin {
            config_name: config_name.to_string(),
            remote_plugin_id: None,
            manifest_name: None,
            plugin_namespace: Some(
                config_name
                    .split_once('@')
                    .map_or(config_name, |(name, _)| name)
                    .to_string(),
            ),
            manifest_description: None,
            root: test_path(config_name),
            enabled: true,
            skill_roots,
            workflow_roots: Vec::new(),
            agent_backends: Vec::new(),
            skill_discovery_mode: SkillDiscoveryMode::Recursive,
            disabled_skill_paths: HashSet::new(),
            has_enabled_skills: true,
            mcp_servers: HashMap::new(),
            apps: Vec::new(),
            hook_sources: Vec::new(),
            hook_load_warnings: Vec::new(),
            error: None,
        }
    }

    #[test]
    fn effective_plugin_workflow_roots_preserve_active_owner_identity() {
        let shared_root = test_path("shared-workflows");
        let mut first = loaded_plugin("zeta@test", Vec::new());
        first.remote_plugin_id = Some("plugins~Plugin_zeta".to_string());
        first.workflow_roots = vec![shared_root.clone()];
        let mut duplicate = loaded_plugin("alpha@test", Vec::new());
        duplicate.workflow_roots = vec![shared_root.clone()];
        let mut disabled = loaded_plugin("disabled@test", Vec::new());
        disabled.enabled = false;
        disabled.workflow_roots = vec![test_path("disabled-workflows")];

        let outcome = PluginLoadOutcome::from_plugins(vec![first, duplicate, disabled]);

        assert_eq!(
            outcome.effective_plugin_workflow_roots(),
            vec![(
                shared_root,
                PluginIdentity {
                    plugin_id: "zeta@test".to_string(),
                    remote_plugin_id: Some("plugins~Plugin_zeta".to_string()),
                },
            )]
        );
    }

    #[test]
    fn effective_plugin_agent_backends_are_namespaced_and_conflicts_are_retained() {
        let declaration = PluginAgentBackendDeclaration {
            local_id: "review".to_string(),
            kind: crate::PluginAgentBackendKind::Acp,
            plugin_version: Some("1.0.0".to_string()),
            source_path: test_path("agent-backends.json"),
            source_digest: "fixture-document-digest".to_string(),
            executable_digest: None,
            source_generation: "fixture-generation".to_string(),
            command: Some(crate::PluginAgentBackendCommand::Name(
                "review-acp".to_string(),
            )),
            command_windows: None,
            args: vec!["--stdio".to_string()],
            env_vars: vec!["REVIEW_TOKEN".to_string()],
            startup_timeout_ms: 20_000,
            run_timeout_ms: None,
            dispose_grace_ms: 3_000,
            max_message_bytes: 8 * 1024 * 1024,
        };
        let mut first = loaded_plugin("first@test", Vec::new());
        first.plugin_namespace = Some("shared".to_string());
        first.remote_plugin_id = Some("plugins~Plugin_first".to_string());
        first.agent_backends = vec![declaration.clone()];
        let mut duplicate = loaded_plugin("second@test", Vec::new());
        duplicate.plugin_namespace = Some("shared".to_string());
        duplicate.agent_backends = vec![declaration.clone()];
        let mut disabled = loaded_plugin("disabled@test", Vec::new());
        disabled.plugin_namespace = Some("hidden".to_string());
        disabled.agent_backends = vec![declaration];
        disabled.enabled = false;

        let outcome = PluginLoadOutcome::from_plugins(vec![first, duplicate, disabled]);
        let backends = outcome.effective_plugin_agent_backends();

        assert_eq!(backends.len(), 2);
        assert_eq!(backends[0].id, "shared/review");
        assert_eq!(backends[0].plugin_identity.plugin_id, "first@test");
        assert_eq!(backends[1].id, "shared/review");
        assert_eq!(backends[1].plugin_identity.plugin_id, "second@test");
    }

    #[test]
    fn effective_plugin_skill_roots_preserves_first_plugin_for_shared_root() {
        let shared_root = test_path("shared-skills");
        let mut first_plugin = loaded_plugin("zeta@test", vec![shared_root.clone()]);
        first_plugin.remote_plugin_id = Some("plugins~Plugin_zeta".to_string());
        let outcome = PluginLoadOutcome::from_plugins(vec![
            first_plugin,
            loaded_plugin("alpha@test", vec![shared_root.clone()]),
        ]);

        assert_eq!(
            outcome.effective_plugin_skill_roots(),
            vec![PluginSkillRoot {
                path: shared_root,
                plugin_identity: PluginIdentity {
                    plugin_id: "zeta@test".to_string(),
                    remote_plugin_id: Some("plugins~Plugin_zeta".to_string()),
                },
                plugin_namespace: "zeta".to_string(),
                plugin_root: test_path("zeta@test"),
                discovery_mode: SkillDiscoveryMode::Recursive,
            }]
        );
    }
}

use std::path::{Path, PathBuf};

use zuno_agent::profile::AgentProfile;
use zuno_catalog::agent;
use zuno_catalog::reference::{ReferenceTarget, ResolvedReferences};
use zuno_catalog::skill::discovery::{SkillOptions, SkillSources};
use zuno_config::schema::Config;
use zuno_config::schema::permission::PermissionAction;
use zuno_permission::{Rule, rules_from_config};

use crate::command::{AgentArgs, AgentCommand};
use crate::environment::StartupEnvironment;

pub(super) fn execute(args: &AgentArgs, environment: &StartupEnvironment) -> Result<(), String> {
    match args
        .command
        .as_ref()
        .ok_or("agent subcommand is required")?
    {
        AgentCommand::List => list(environment),
    }
}

fn list(environment: &StartupEnvironment) -> Result<(), String> {
    let directory = std::env::current_dir().map_err(|error| error.to_string())?;
    let project = zuno_paths::project::resolve_project(&directory);
    let worktree = project.vcs.as_ref().map(|_| project.directory.as_path());
    let env = environment.resolved();
    let config = zuno_config::discovery::discover_with(
        &zuno_config::discovery::DiscoveryOptions::new(&directory, worktree, env.clone()),
    )
    .map_err(|error| error.report())?;
    let extension_scope = zuno_extension::Scope::new(worktree.unwrap_or(directory.as_path()));
    let static_extensions = zuno_extension::discover_static(&directory, worktree, env)
        .map_err(|error| error.to_string())?;
    let extensions = zuno_extension::resolve_active(
        &extension_scope,
        &static_extensions,
        environment.extensions(),
    )
    .map_err(|error| error.to_string())?;
    let loaded = agent::load_map(&directory, worktree, env).map_err(|error| error.to_string())?;
    let merged = agent::merge_agent_maps(&loaded.agents, extensions.agents())
        .map_err(|error| error.to_string())?;
    let agents = agent::list(&merged, &loaded.origins);
    let dynamic = DynamicRules::resolve(&directory, worktree, env, &config);

    for entry in agents {
        let profile = resolved_profile(entry, &config, &dynamic, true);
        println!("{}", profile.definition().header());
        println!(
            "  {}",
            serde_json::to_string_pretty(profile.capabilities().rules())
                .map_err(|error| error.to_string())?
        );
    }
    Ok(())
}

/// The rule patterns that cannot be written down ahead of time.
///
/// They name resolved paths — the tool-output directory, the temp directory, the
/// discovered skill and reference directories — so they are
/// computed once per invocation and then handed to [`resolved_rules`]. This is
/// `pub(crate)` because `run` needs the same ruleset the listing prints: a
/// permission set that a user can read with `agent list` but that the turn loop
/// does not actually enforce would be worse than having no listing at all.
pub(crate) struct DynamicRules {
    readonly_external: Vec<Rule>,
    truncate_glob: String,
}

impl DynamicRules {
    pub(crate) fn resolve(
        directory: &Path,
        worktree: Option<&Path>,
        env: &zuno_paths::Env,
        config: &Config,
    ) -> Self {
        let layout = zuno_paths::Layout::resolve(env);
        let truncate_glob = glob(&layout.tool_output());
        let mut whitelisted = vec![truncate_glob.clone(), glob(layout.temp())];

        let skills =
            SkillSources::discover(&SkillOptions::from_config(directory, worktree, env, config));
        whitelisted.extend(skills.dirs().iter().map(|path| glob(path)));
        whitelisted.extend(
            reference_directories(config, directory)
                .iter()
                .map(|path| glob(path)),
        );

        let mut readonly_external = vec![rule("external_directory", "*", PermissionAction::Ask)];
        readonly_external.extend(
            whitelisted
                .into_iter()
                .map(|pattern| rule("external_directory", &pattern, PermissionAction::Allow)),
        );

        Self {
            readonly_external,
            truncate_glob,
        }
    }
}

struct ResolvedRules {
    rules: Vec<Rule>,
    extension_rule_index: usize,
}

fn resolved_rule_set(
    entry: &agent::Agent,
    config: &Config,
    dynamic: &DynamicRules,
) -> ResolvedRules {
    let mut rules = default_rules(dynamic);

    if entry.source.is_native()
        && let Some(builtin) = agent::builtin::get(&entry.name)
        && let Some(overlay) = builtin.permission_overlay()
    {
        rules.extend(rules_from_config(&overlay));
        match entry.name.as_str() {
            "plan" | "explorer" | "librarian" | "oracle" | "looker" => {
                rules.extend(dynamic.readonly_external.clone())
            }
            _ => {}
        }
    }

    // Extension grants belong to the native role layer. User and per-Agent rules
    // follow this boundary and therefore retain final authority to deny them.
    let extension_rule_index = rules.len();
    if let Some(user) = &config.permission {
        rules.extend(rules_from_config(user));
    }
    if let Some(agent_rules) = &entry.permission {
        rules.extend(rules_from_config(agent_rules));
    }

    let truncate_explicitly_denied = rules.iter().any(|candidate| {
        candidate.permission == "external_directory"
            && candidate.pattern == dynamic.truncate_glob
            && candidate.action == PermissionAction::Deny
    });
    if !truncate_explicitly_denied {
        rules.push(rule(
            "external_directory",
            &dynamic.truncate_glob,
            PermissionAction::Allow,
        ));
    }
    ResolvedRules {
        rules,
        extension_rule_index,
    }
}

/// Freeze one catalog entry together with the exact rules this process enforces.
pub(crate) fn resolved_profile(
    entry: agent::Agent,
    config: &Config,
    dynamic: &DynamicRules,
    vision_available: bool,
) -> AgentProfile {
    let resolved = resolved_rule_set(&entry, config, dynamic);
    AgentProfile::resolve_with_extension_boundary(
        entry,
        resolved.rules,
        resolved.extension_rule_index,
        vision_available,
    )
}

fn default_rules(dynamic: &DynamicRules) -> Vec<Rule> {
    let mut rules = vec![
        rule("*", "*", PermissionAction::Allow),
        rule("doom_loop", "*", PermissionAction::Ask),
        rule("external_directory", "*", PermissionAction::Ask),
    ];
    rules.extend(dynamic.readonly_external.iter().skip(1).cloned());
    rules.extend([
        rule("question", "*", PermissionAction::Deny),
        rule("plan_enter", "*", PermissionAction::Deny),
        rule("plan_exit", "*", PermissionAction::Deny),
        rule("read", "*", PermissionAction::Allow),
        rule("read", "*.env", PermissionAction::Ask),
        rule("read", "*.env.*", PermissionAction::Ask),
        rule("read", "*.env.example", PermissionAction::Allow),
    ]);
    rules
}

fn reference_directories(config: &Config, directory: &Path) -> Vec<PathBuf> {
    let declared = config.references.as_ref();
    ResolvedReferences::resolve(declared)
        .iter()
        .filter_map(|reference| match &reference.target {
            ReferenceTarget::Local { path } => {
                let path = PathBuf::from(path);
                Some(if path.is_absolute() {
                    path
                } else {
                    directory.join(path)
                })
            }
            ReferenceTarget::Shorthand(_) | ReferenceTarget::Git { .. } => None,
        })
        .collect()
}

fn glob(path: &Path) -> String {
    path.join("*").to_string_lossy().into_owned()
}

fn rule(permission: &str, pattern: &str, action: PermissionAction) -> Rule {
    Rule {
        permission: permission.to_owned(),
        pattern: pattern.to_owned(),
        action,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_rules_preserve_find_last_order() {
        let dynamic = DynamicRules {
            readonly_external: vec![
                rule("external_directory", "*", PermissionAction::Ask),
                rule(
                    "external_directory",
                    "/tmp/opencode/*",
                    PermissionAction::Allow,
                ),
            ],
            truncate_glob: "/data/tool-output/*".to_owned(),
        };
        let rules = default_rules(&dynamic);
        assert_eq!(rules[0].permission, "*");
        assert_eq!(rules[2].permission, "external_directory");
        assert_eq!(rules[3].pattern, "/tmp/opencode/*");
        assert_eq!(rules.last().expect("last rule").pattern, "*.env.example");
    }
}

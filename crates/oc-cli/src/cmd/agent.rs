use std::path::{Component, Path, PathBuf};

use oc_catalog::agent;
use oc_catalog::reference::{ReferenceTarget, ResolvedReferences};
use oc_catalog::skill::discovery::{SkillOptions, SkillSources};
use oc_config::schema::Config;
use oc_config::schema::permission::PermissionAction;
use oc_permission::{Rule, rules_from_config};

use crate::command::{AgentArgs, AgentCommand};
use crate::environment::StartupEnvironment;

pub(super) fn execute(args: &AgentArgs, environment: &StartupEnvironment) -> Result<(), String> {
    match args
        .command
        .as_ref()
        .ok_or("agent subcommand is required")?
    {
        AgentCommand::List => list(environment),
        AgentCommand::Create(_) => Err(
            "agent creation requires the model-backed generator, which is not available yet"
                .to_owned(),
        ),
    }
}

fn list(environment: &StartupEnvironment) -> Result<(), String> {
    let directory = std::env::current_dir().map_err(|error| error.to_string())?;
    let project = oc_paths::project::resolve_project(&directory);
    let worktree = project.vcs.as_ref().map(|_| project.directory.as_path());
    let env = environment.resolved();
    let config = oc_config::discovery::discover_with(&oc_config::discovery::DiscoveryOptions::new(
        &directory,
        worktree,
        env.clone(),
    ))
    .map_err(|error| error.report())?;
    let agents = agent::load(&directory, worktree, env).map_err(|error| error.to_string())?;
    let dynamic = DynamicRules::resolve(&directory, worktree, env, &config);

    for entry in agents {
        println!("{}", entry.header());
        let rules = resolved_rules(&entry, &config, &dynamic);
        println!(
            "  {}",
            serde_json::to_string_pretty(&rules).map_err(|error| error.to_string())?
        );
    }
    Ok(())
}

/// The rule patterns that cannot be written down ahead of time.
///
/// They name resolved paths — the tool-output directory, the temp directory, the
/// discovered skill and reference directories, the plan directory — so they are
/// computed once per invocation and then handed to [`resolved_rules`]. This is
/// `pub(crate)` because `run` needs the same ruleset the listing prints: a
/// permission set that a user can read with `agent list` but that the turn loop
/// does not actually enforce would be worse than having no listing at all.
pub(crate) struct DynamicRules {
    readonly_external: Vec<Rule>,
    truncate_glob: String,
    plan_directory_glob: String,
    relative_plan_glob: String,
}

impl DynamicRules {
    pub(crate) fn resolve(
        directory: &Path,
        worktree: Option<&Path>,
        env: &oc_paths::Env,
        config: &Config,
    ) -> Self {
        let layout = oc_paths::Layout::resolve(env);
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

        let plan_directory_glob = glob(&layout.data().join("plans"));
        let absolute_plan_glob = layout.data().join("plans").join("*.md");
        let relative_plan_glob = worktree.map_or_else(
            || absolute_plan_glob.to_string_lossy().into_owned(),
            |root| relative_path(root, &absolute_plan_glob),
        );

        Self {
            readonly_external,
            truncate_glob,
            plan_directory_glob,
            relative_plan_glob,
        }
    }
}

pub(crate) fn resolved_rules(
    entry: &agent::Agent,
    config: &Config,
    dynamic: &DynamicRules,
) -> Vec<Rule> {
    let mut rules = default_rules(dynamic);

    if entry.source.is_native()
        && let Some(builtin) = agent::builtin::get(&entry.name)
        && let Some(overlay) = builtin.permission_overlay()
    {
        rules.extend(rules_from_config(&overlay));
        match entry.name.as_str() {
            "plan" => {
                rules.push(rule(
                    "external_directory",
                    &dynamic.plan_directory_glob,
                    PermissionAction::Allow,
                ));
                rules.extend([
                    rule("edit", "*", PermissionAction::Deny),
                    rule("edit", ".opencode/plans/*.md", PermissionAction::Allow),
                    rule("edit", &dynamic.relative_plan_glob, PermissionAction::Allow),
                ]);
            }
            "explore" => rules.extend(dynamic.readonly_external.clone()),
            _ => {}
        }
    }

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
    rules
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
    let declared = config.references.as_ref().or(config.reference.as_ref());
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

fn relative_path(from: &Path, to: &Path) -> String {
    let from: Vec<Component<'_>> = from.components().collect();
    let to: Vec<Component<'_>> = to.components().collect();
    let shared = from
        .iter()
        .zip(&to)
        .take_while(|(left, right)| left == right)
        .count();
    let mut relative = PathBuf::new();
    for _ in shared..from.len() {
        relative.push("..");
    }
    for component in &to[shared..] {
        relative.push(component.as_os_str());
    }
    relative.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_plan_path_matches_node_path_relative_shape() {
        assert_eq!(
            relative_path(
                Path::new("/work/repo"),
                Path::new("/home/user/.local/share/opencode/plans/*.md")
            ),
            "../../home/user/.local/share/opencode/plans/*.md"
        );
    }

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
            plan_directory_glob: "/data/plans/*".to_owned(),
            relative_plan_glob: "../data/plans/*.md".to_owned(),
        };
        let rules = default_rules(&dynamic);
        assert_eq!(rules[0].permission, "*");
        assert_eq!(rules[2].permission, "external_directory");
        assert_eq!(rules[3].pattern, "/tmp/opencode/*");
        assert_eq!(rules.last().expect("last rule").pattern, "*.env.example");
    }
}

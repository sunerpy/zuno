//! Zuno's native agents, `agent.<name>` config entries, and Markdown definitions
//! discovered under `{agent,agents}/**/*.md`.
//!
//! # Name rule
//!
//! An agent's name is its path relative to the config directory, minus the
//! `agent/` or `agents/` prefix and minus the extension. So
//! `agent/review/security.md` is the agent `review/security` — **not** `security`.
//!
//! A frontmatter `name:` key overrides the derived name entirely, because the
//! resolved map is keyed by the final name. `agent/original.md` with
//! `name: renamed-by-frontmatter` therefore exposes only
//! `renamed-by-frontmatter`.
//!
//! # Layer order
//!
//! Global Zuno config, explicit config, project files, and `.zuno` directories
//! are merged before Markdown agents. `ZUNO_CONFIG_CONTENT` is re-applied after
//! Markdown so the explicit environment layer remains authoritative.
//!
//! # What is deliberately not here
//!
//! * `{mode,modes}/*.md` is not scanned. Zuno reads only `agent/` and `agents/`.
//! * `tools` and `maxSteps` are not accepted. [`AgentConfig`] rejects them as
//!   unsupported fields.
//! * The unknown-key sweep into `options` is not reimplemented.
//!   [`zuno_config::schema::agent::AgentConfig`]'s `Deserialize` performs it, and
//!   this module simply consumes the result.
//! * Permissions are not resolved into a ruleset. The `permission` key is carried
//!   verbatim and the built-in overlays are exposed as data; see
//!   [`builtin::Builtin::permission_overlay`].

pub mod builtin;
pub mod frontmatter;

use serde::Serialize;
use serde_json::Value;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use zuno_config::discovery::{DiscoveryOptions, discover_with};
use zuno_config::schema::agent::{AgentColor, AgentConfig};
use zuno_config::schema::ordered::OrderedMap;
use zuno_config::schema::permission::PermissionConfig;
use zuno_config::schema::{Config, JsonMap};
use zuno_error::ConfigError;
use zuno_paths::{Env, Layout};

pub use zuno_config::schema::agent::AgentMode;

/// Prefixes stripped from a Markdown agent's relative path.
///
/// Order matters: the first match wins.
pub const AGENT_DIRECTORY_PREFIXES: [&str; 2] = ["agent/", "agents/"];

/// The Markdown layout scanned in every Zuno config directory.
///
/// Reproduced here as documentation; the walk is implemented directly because
/// `{a,b}` brace expansion plus `**` plus dotfile inclusion plus symlink following
/// is clearer as two explicit roots than as a glob dependency.
pub const AGENT_GLOB: &str = "{agent,agents}/**/*.md";

/// Where an agent's definition came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSource {
    /// One of the eleven native agents, with no user definition on top.
    Native,
    /// A native agent a user definition has modified.
    NativeOverridden,
    /// An `agent.<name>` entry in a config file or env layer.
    Config,
    /// A Markdown file, named by its path.
    Markdown {
        /// The file the definition was read from.
        path: PathBuf,
    },
}

impl AgentSource {
    /// Whether this is a native agent, overridden or not.
    ///
    /// `agent list` sorts natives before everything else, and an override does not
    /// change that classification.
    #[must_use]
    pub fn is_native(&self) -> bool {
        matches!(self, Self::Native | Self::NativeOverridden)
    }
}

/// A resolved agent, after built-ins, config entries, and Markdown definitions
/// have been folded together.
///
/// The catalog representation before runtime permissions are resolved.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Agent {
    /// The agent's name, which is also how a user selects it.
    pub name: String,
    /// When to use the agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Where the agent may be used. A newly defined agent defaults to
    /// [`AgentMode::All`].
    pub mode: AgentMode,
    /// Hidden from the `@` autocomplete menu.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    /// Model in `provider/model` form, unparsed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Model variant, applied only with the agent's own model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    /// Sampling temperature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus-sampling cutoff, serialized as `topP`.
    #[serde(rename = "topP", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Display colour.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<AgentColor>,
    /// The system prompt. For a Markdown agent this is the trimmed body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Maximum agentic iterations before a text-only response is forced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<NonZeroU32>,
    /// Provider options, including every unknown key swept in by
    /// [`AgentConfig`]'s deserializer.
    pub options: JsonMap,
    /// The `permission` key verbatim, unresolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission: Option<PermissionConfig>,
    /// Where the definition came from.
    pub source: AgentSource,
}

impl Agent {
    /// The `name (mode)` header `agent list` prints.
    #[must_use]
    pub fn header(&self) -> String {
        format!("{} ({})", self.name, mode_label(self.mode))
    }
}

/// The lowercase mode name the CLI prints and config accepts.
#[must_use]
pub fn mode_label(mode: AgentMode) -> &'static str {
    match mode {
        AgentMode::Subagent => "subagent",
        AgentMode::Primary => "primary",
        AgentMode::All => "all",
    }
}

/// One Markdown agent definition as read from disk.
#[derive(Debug, Clone, PartialEq)]
pub struct MarkdownAgent {
    /// The name, derived from the path or overridden by a frontmatter `name:`.
    pub name: String,
    /// The file it was read from.
    pub path: PathBuf,
    /// The frontmatter fields, with the body already installed as `prompt`.
    pub config: AgentConfig,
}

/// Which file each Markdown-defined agent was read from, keyed by agent name.
///
/// Carried alongside the agent map so provenance metadata does not participate in
/// the user-config deep merge.
pub type MarkdownOrigins = Vec<(String, PathBuf)>;

/// The merged agent map and the provenance of its Markdown-defined entries.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LoadedAgents {
    /// Every `agent.<name>` entry, after all layers have merged.
    pub agents: OrderedMap<AgentConfig>,
    /// Where each Markdown-defined agent came from.
    pub origins: MarkdownOrigins,
}

/// Derive an agent name from a path relative to one scanned config directory.
///
/// Prefix matching is anchored. When no prefix matches, the basename is used so
/// an unexpected path cannot retain unrelated parent segments.
#[must_use]
pub fn entry_name_from_path(relative: &str, prefixes: &[&str]) -> String {
    let normalized = relative.replace('\\', "/");
    let candidate = prefixes
        .iter()
        .find_map(|prefix| normalized.strip_prefix(prefix))
        .map_or_else(|| basename(&normalized).to_owned(), str::to_owned);
    strip_extension(&candidate)
}

/// `path.basename` over a `/`-separated string.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `path.extname` semantics: only a dot after at least one character in the final
/// segment counts, so `.hidden` keeps its whole name while `a.md` loses `.md`.
fn strip_extension(candidate: &str) -> String {
    let segment = basename(candidate);
    let Some(dot) = segment.rfind('.') else {
        return candidate.to_owned();
    };
    if dot == 0 {
        return candidate.to_owned();
    }
    let cut = candidate.len() - (segment.len() - dot);
    candidate[..cut].to_owned()
}

/// Read every Markdown agent under one config directory.
///
/// Scans `agent/` and `agents/`, recursively, including dot-directories and
/// following symlinks.
/// Files are visited in sorted order so a directory listing's arbitrary order
/// cannot decide which of two same-named definitions wins.
///
/// A file whose frontmatter cannot be parsed is skipped with a warning. A
/// document that parses but violates the Zuno agent schema is fatal.
///
/// # Errors
///
/// [`ConfigError::Io`] when a directory or file cannot be read, and
/// [`ConfigError::Invalid`] when a definition violates the agent schema.
pub fn discover_in_directory(dir: &Path) -> Result<Vec<MarkdownAgent>, ConfigError> {
    let mut found = Vec::new();
    for root_name in ["agent", "agents"] {
        let root = dir.join(root_name);
        if !root.is_dir() {
            continue;
        }
        for file in markdown_files(&root)? {
            let Some(agent) = read_markdown_agent(dir, &file)? else {
                continue;
            };
            found.push(agent);
        }
    }
    Ok(found)
}

/// Every `*.md` under `root`, recursively, sorted, following symlinks.
pub(crate) fn markdown_files(root: &Path) -> Result<Vec<PathBuf>, ConfigError> {
    let mut files = Vec::new();
    let mut queue = vec![root.to_path_buf()];
    let mut visited = Vec::new();
    while let Some(dir) = queue.pop() {
        // Following symlinks means a cycle is reachable; canonicalizing each
        // visited directory bounds the walk without refusing legitimate symlinked
        // agent trees.
        let key = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        if visited.contains(&key) {
            continue;
        }
        visited.push(key);

        let entries = std::fs::read_dir(&dir).map_err(|source| ConfigError::Io {
            path: dir.clone(),
            source,
        })?;
        let mut local = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| ConfigError::Io {
                path: dir.clone(),
                source,
            })?;
            local.push(entry.path());
        }
        local.sort();
        for path in local {
            if path.is_dir() {
                queue.push(path);
            } else if path.extension().is_some_and(|ext| ext == "md") {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

/// Parse one Markdown agent, returning `None` when its frontmatter is unreadable.
///
/// # Errors
///
/// [`ConfigError::Io`] when the file cannot be read, and [`ConfigError::Invalid`]
/// when the frontmatter does not satisfy the native agent schema.
pub fn read_markdown_agent(dir: &Path, file: &Path) -> Result<Option<MarkdownAgent>, ConfigError> {
    let text = std::fs::read_to_string(file).map_err(|source| ConfigError::Io {
        path: file.to_path_buf(),
        source,
    })?;

    // An unparseable head takes the file out of the list rather than failing the
    // whole load.
    let Ok(document) = frontmatter::parse(&text) else {
        tracing::warn!(
            path = %file.display(),
            "skipping agent: its frontmatter could not be parsed"
        );
        return Ok(None);
    };

    let relative = relative_path(dir, file);
    let derived = entry_name_from_path(&relative, &AGENT_DIRECTORY_PREFIXES);

    let mut object = document.data;
    // The derived name is a default a frontmatter `name:` may replace, while the
    // body always wins over a frontmatter `prompt:`.
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .map_or(derived, str::to_owned);
    object.insert("name".to_owned(), Value::String(name.clone()));
    object
        .entry("options".to_owned())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    object
        .entry("permission".to_owned())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    object.insert(
        "prompt".to_owned(),
        Value::String(document.content.trim().to_owned()),
    );

    let value = Value::Object(object);
    let config: AgentConfig =
        serde_json::from_value(value).map_err(|source| ConfigError::Invalid {
            path: file.to_path_buf(),
            issues: vec![zuno_error::ConfigIssue::new(
                ["agent", name.as_str()],
                source.to_string(),
            )],
        })?;

    Ok(Some(MarkdownAgent {
        name,
        path: file.to_path_buf(),
        config,
    }))
}

/// `path.relative(dir, item)` with `/` separators, for the name rule.
pub(crate) fn relative_path(dir: &Path, file: &Path) -> String {
    file.strip_prefix(dir)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The Markdown agent layer across every config directory, in discovery order.
///
/// A later directory wins. [`zuno_paths::Layout::config_directories`] returns
/// directories in that precedence order.
///
/// # Errors
///
/// Propagates the first [`ConfigError`] from [`discover_in_directory`].
pub fn discover_markdown(dirs: &[PathBuf]) -> Result<Vec<MarkdownAgent>, ConfigError> {
    let mut found = Vec::new();
    for dir in dirs {
        found.extend(discover_in_directory(dir)?);
    }
    Ok(found)
}

/// Deep-merge one agent map over another.
///
/// Merging happens on the JSON representation so an `options` map or nested
/// `permission` object merges key-by-key rather than being replaced wholesale.
///
/// # Errors
///
/// [`ConfigError::Invalid`] when a merged entry no longer satisfies the agent
/// schema, naming the agent.
pub fn merge_agent_maps(
    base: &OrderedMap<AgentConfig>,
    overlay: &OrderedMap<AgentConfig>,
) -> Result<OrderedMap<AgentConfig>, ConfigError> {
    let mut merged = OrderedMap::new();
    for (name, config) in base.iter() {
        merged.insert(name, config.clone());
    }
    for (name, config) in overlay.iter() {
        let next = match merged.get(name) {
            Some(existing) => merge_one(name, existing, config)?,
            None => config.clone(),
        };
        merged.insert(name, next);
    }
    Ok(merged)
}

fn merge_one(
    name: &str,
    base: &AgentConfig,
    overlay: &AgentConfig,
) -> Result<AgentConfig, ConfigError> {
    let mut merged = to_value(name, base)?;
    merge_deep(&mut merged, to_value(name, overlay)?);
    serde_json::from_value(merged).map_err(|source| ConfigError::Invalid {
        path: PathBuf::from(format!("agent.{name}")),
        issues: vec![zuno_error::ConfigIssue::new(
            ["agent", name],
            source.to_string(),
        )],
    })
}

fn to_value(name: &str, config: &AgentConfig) -> Result<Value, ConfigError> {
    serde_json::to_value(config).map_err(|source| ConfigError::Invalid {
        path: PathBuf::from(format!("agent.{name}")),
        issues: vec![zuno_error::ConfigIssue::new(
            ["agent", name],
            source.to_string(),
        )],
    })
}

/// remeda's `mergeDeep`: objects merge key-by-key, everything else is replaced.
fn merge_deep(target: &mut Value, source: Value) {
    match (target, source) {
        (Value::Object(target), Value::Object(source)) => {
            for (key, value) in source {
                match target.get_mut(&key) {
                    Some(existing) => merge_deep(existing, value),
                    None => {
                        target.insert(key, value);
                    }
                }
            }
        }
        (target, source) => *target = source,
    }
}

/// Fold an agent map over the eleven native agents.
///
/// * `disable: true` removes the agent, native or not.
/// * A name that is not native becomes a new agent with `mode: "all"`.
/// * An absent key leaves the native value in place.
/// * `options` deep-merges rather than replacing.
///
/// The returned order is the eleven native agents first, then new agents in map
/// order. [`list`] applies the display sort separately.
#[must_use]
pub fn resolve(agents: &OrderedMap<AgentConfig>, origins: &[(String, PathBuf)]) -> Vec<Agent> {
    let mut resolved: Vec<Agent> = builtin::all().into_iter().map(from_builtin).collect();

    for (name, config) in agents.iter() {
        if config.disable == Some(true) {
            resolved.retain(|agent| agent.name != name);
            continue;
        }
        let position = resolved.iter().position(|agent| agent.name == name);
        let index = match position {
            Some(index) => index,
            None => {
                resolved.push(new_agent(name));
                resolved.len() - 1
            }
        };
        apply(&mut resolved[index], config);
        if let Some((_, path)) = origins.iter().find(|(key, _)| key == name) {
            resolved[index].source = match &resolved[index].source {
                source if source.is_native() => AgentSource::NativeOverridden,
                _ => AgentSource::Markdown { path: path.clone() },
            };
        }
    }
    resolved
}

fn from_builtin(builtin: builtin::Builtin) -> Agent {
    Agent {
        name: builtin.name.to_owned(),
        description: builtin.description.map(str::to_owned),
        mode: builtin.mode,
        hidden: builtin.hidden.then_some(true),
        model: None,
        variant: None,
        temperature: builtin.temperature,
        top_p: None,
        color: None,
        prompt: builtin.prompt.map(str::to_owned),
        steps: None,
        options: JsonMap::new(),
        permission: None,
        source: AgentSource::Native,
    }
}

/// A name with no native definition becomes a primary-and-subagent agent.
fn new_agent(name: &str) -> Agent {
    Agent {
        name: name.to_owned(),
        description: None,
        mode: AgentMode::All,
        hidden: None,
        model: None,
        variant: None,
        temperature: None,
        top_p: None,
        color: None,
        prompt: None,
        steps: None,
        options: JsonMap::new(),
        permission: None,
        source: AgentSource::Config,
    }
}

/// Apply one user overlay while preserving absent native fields.
fn apply(agent: &mut Agent, config: &AgentConfig) {
    if agent.source == AgentSource::Native {
        agent.source = AgentSource::NativeOverridden;
    }
    if let Some(model) = &config.model {
        agent.model = Some(model.clone());
    }
    if let Some(variant) = &config.variant {
        agent.variant = Some(variant.clone());
    }
    if let Some(prompt) = &config.prompt {
        agent.prompt = Some(prompt.clone());
    }
    if let Some(description) = &config.description {
        agent.description = Some(description.clone());
    }
    if let Some(temperature) = config.temperature {
        agent.temperature = Some(temperature);
    }
    if let Some(top_p) = config.top_p {
        agent.top_p = Some(top_p);
    }
    if let Some(mode) = config.mode {
        agent.mode = mode;
    }
    if let Some(color) = &config.color {
        agent.color = Some(color.clone());
    }
    if let Some(hidden) = config.hidden {
        agent.hidden = Some(hidden);
    }
    if let Some(steps) = config.steps {
        agent.steps = Some(steps);
    }

    // A `name` key renames the resolved agent while the config lookup key remains
    // unchanged.
    if let Some(name) = config.extra.get("name").and_then(Value::as_str) {
        agent.name = name.to_owned();
    }

    if let Some(options) = &config.options {
        let mut merged = Value::Object(std::mem::take(&mut agent.options));
        merge_deep(&mut merged, Value::Object(options.clone()));
        if let Value::Object(map) = merged {
            agent.options = map;
        }
    }

    if let Some(permission) = &config.permission {
        agent.permission = Some(permission.clone());
    }
}

/// Every agent in Zuno CLI display order: natives first, then name order.
#[must_use]
pub fn list(agents: &OrderedMap<AgentConfig>, origins: &[(String, PathBuf)]) -> Vec<Agent> {
    let mut resolved = resolve(agents, origins);
    resolved.sort_by(|left, right| {
        left.source
            .is_native()
            .cmp(&right.source.is_native())
            .reverse()
            .then_with(|| locale_compare(&left.name, &right.name))
    });
    resolved
}

/// Case-insensitive primary ordering for agent names.
///
/// Agent names come from file paths and config keys, so the primary key is the
/// lowercase name and the raw name breaks ties deterministically.
fn locale_compare(left: &str, right: &str) -> std::cmp::Ordering {
    left.to_lowercase()
        .cmp(&right.to_lowercase())
        .then_with(|| left.cmp(right))
}

/// Discover and resolve every agent for one working directory.
///
/// Assembles merged Zuno config, Markdown definitions, then
/// `ZUNO_CONFIG_CONTENT` re-applied on top so the explicit environment layer
/// remains authoritative.
///
/// # Errors
///
/// Propagates [`ConfigError`] from config discovery, from reading an agent
/// directory, or from a Markdown definition that violates the agent schema.
pub fn load(
    directory: &Path,
    worktree: Option<&Path>,
    env: &Env,
) -> Result<Vec<Agent>, ConfigError> {
    let loaded = load_map(directory, worktree, env)?;
    Ok(list(&loaded.agents, &loaded.origins))
}

/// The merged agent map plus the Markdown provenance [`list`] needs.
///
/// Exposed so a caller that wants the map — the turn loop resolving one agent by
/// name — does not have to build and sort the whole list.
///
/// # Errors
///
/// As [`load`].
pub fn load_map(
    directory: &Path,
    worktree: Option<&Path>,
    env: &Env,
) -> Result<LoadedAgents, ConfigError> {
    let options = DiscoveryOptions::new(directory, worktree, env.clone());
    let config = discover_with(&options)?;
    let base = config.agent.clone().unwrap_or_default();

    let layout = Layout::resolve(env);
    let dirs = layout.config_directories(directory, worktree);
    let markdown = discover_markdown(&dirs)?;

    let mut overlay = OrderedMap::new();
    let mut origins: MarkdownOrigins = Vec::with_capacity(markdown.len());
    for agent in markdown {
        overlay.insert(agent.name.clone(), agent.config);
        origins.retain(|(name, _)| name != &agent.name);
        origins.push((agent.name, agent.path));
    }

    let mut agents = merge_agent_maps(&base, &overlay)?;
    if let Some(text) = env.truthy_value("ZUNO_CONFIG_CONTENT")
        && let Ok(layer) = serde_json::from_str::<Config>(text)
        && let Some(from_env) = layer.agent
    {
        agents = merge_agent_maps(&agents, &from_env)?;
        for (name, _) in from_env.iter() {
            origins.retain(|(key, _)| key != name);
        }
    }

    Ok(LoadedAgents { agents, origins })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(json: serde_json::Value) -> OrderedMap<AgentConfig> {
        serde_json::from_value(json).expect("agent map fixture should deserialize")
    }

    fn names(agents: &[Agent]) -> Vec<&str> {
        agents.iter().map(|agent| agent.name.as_str()).collect()
    }

    #[test]
    fn the_name_is_the_relative_path_minus_prefix_and_extension() {
        assert_eq!(
            entry_name_from_path("agent/review/security.md", &AGENT_DIRECTORY_PREFIXES),
            "review/security"
        );
        assert_eq!(
            entry_name_from_path("agents/deep/nested/thing.md", &AGENT_DIRECTORY_PREFIXES),
            "deep/nested/thing"
        );
        assert_eq!(
            entry_name_from_path("agent/flat.md", &AGENT_DIRECTORY_PREFIXES),
            "flat"
        );
    }

    #[test]
    fn a_windows_separator_normalizes_before_the_prefix_match() {
        assert_eq!(
            entry_name_from_path("agent\\review\\security.md", &AGENT_DIRECTORY_PREFIXES),
            "review/security"
        );
    }

    #[test]
    fn a_path_with_no_prefix_falls_back_to_its_basename() {
        // The prefix is anchored, so a coincidental `agent` segment deeper in the
        // path does not key the agent.
        assert_eq!(
            entry_name_from_path("other/agent/security.md", &AGENT_DIRECTORY_PREFIXES),
            "security"
        );
    }

    #[test]
    fn only_the_final_extension_is_stripped_and_a_dotfile_keeps_its_name() {
        assert_eq!(
            entry_name_from_path("agent/a.tar.md", &AGENT_DIRECTORY_PREFIXES),
            "a.tar"
        );
        assert_eq!(
            entry_name_from_path("agent/.hidden", &AGENT_DIRECTORY_PREFIXES),
            ".hidden"
        );
        assert_eq!(
            entry_name_from_path("agent/dir/.hidden", &AGENT_DIRECTORY_PREFIXES),
            "dir/.hidden"
        );
    }

    #[test]
    fn the_first_matching_prefix_wins() {
        // `agent/` is checked before `agents/`, so a path under `agents/` whose
        // next segment is also `agent` is not double-stripped.
        assert_eq!(
            entry_name_from_path("agents/agent/x.md", &AGENT_DIRECTORY_PREFIXES),
            "agent/x"
        );
    }

    #[test]
    fn the_native_roster_exists_with_no_user_config_at_all() {
        let agents = resolve(&OrderedMap::new(), &[]);
        assert_eq!(
            names(&agents),
            vec![
                "build",
                "plan",
                "deep",
                "explorer",
                "librarian",
                "advisor",
                "worker",
                "looker",
                "compaction",
                "title",
                "summary"
            ]
        );
        assert!(
            agents
                .iter()
                .all(|agent| agent.source == AgentSource::Native)
        );
    }

    #[test]
    fn a_new_agent_defaults_to_mode_all() {
        let agents = resolve(&map(serde_json::json!({ "fresh": {} })), &[]);
        let fresh = agents
            .iter()
            .find(|agent| agent.name == "fresh")
            .expect("the new agent should exist");
        assert_eq!(fresh.mode, AgentMode::All);
        assert_eq!(fresh.source, AgentSource::Config);
    }

    #[test]
    fn a_user_definition_overrides_a_built_in_and_it_stays_native() {
        let agents = resolve(
            &map(serde_json::json!({
                "plan": { "description": "overridden plan", "mode": "all" },
            })),
            &[],
        );
        let plan = agents
            .iter()
            .find(|agent| agent.name == "plan")
            .expect("plan should still exist");
        assert_eq!(plan.mode, AgentMode::All);
        assert_eq!(plan.description.as_deref(), Some("overridden plan"));
        assert_eq!(plan.source, AgentSource::NativeOverridden);
        assert!(
            plan.source.is_native(),
            "an override stays in the native block"
        );
    }

    #[test]
    fn an_override_that_omits_a_field_keeps_the_built_ins_value() {
        let agents = resolve(
            &map(serde_json::json!({ "title": { "model": "anthropic/haiku" } })),
            &[],
        );
        let title = agents
            .iter()
            .find(|agent| agent.name == "title")
            .expect("title exists");
        assert_eq!(title.model.as_deref(), Some("anthropic/haiku"));
        assert_eq!(
            title.temperature,
            Some(0.5),
            "the built-in temperature survives"
        );
        assert_eq!(title.prompt.as_deref(), Some(builtin::PROMPT_TITLE));
        assert_eq!(title.hidden, Some(true));
    }

    #[test]
    fn disable_removes_an_agent_entirely() {
        let agents = resolve(
            &map(serde_json::json!({ "plan": { "disable": true } })),
            &[],
        );
        assert!(!names(&agents).contains(&"plan"));
        assert_eq!(agents.len(), 10);
    }

    #[test]
    fn unknown_frontmatter_keys_land_in_options_via_the_config_schema() {
        // The sweep itself belongs to zuno-config; this asserts it survives the fold.
        let agents = resolve(
            &map(serde_json::json!({
                "custom": { "reasoningEffort": "high", "thinking": { "type": "enabled" } },
            })),
            &[],
        );
        let custom = agents
            .iter()
            .find(|agent| agent.name == "custom")
            .expect("custom exists");
        assert_eq!(
            custom.options.get("reasoningEffort"),
            Some(&Value::String("high".to_owned()))
        );
        assert!(custom.options.contains_key("thinking"));
    }

    #[test]
    fn options_deep_merge_rather_than_replacing() {
        let base = map(serde_json::json!({
            "custom": { "options": { "keep": 1, "nested": { "a": 1 } } },
        }));
        let overlay = map(serde_json::json!({
            "custom": { "options": { "nested": { "b": 2 } } },
        }));
        let merged = merge_agent_maps(&base, &overlay).expect("merge succeeds");
        let agents = resolve(&merged, &[]);
        let custom = agents
            .iter()
            .find(|agent| agent.name == "custom")
            .expect("custom exists");
        assert_eq!(custom.options.get("keep"), Some(&Value::from(1)));
        assert_eq!(
            custom.options.get("nested"),
            Some(&serde_json::json!({ "a": 1, "b": 2 }))
        );
    }

    #[test]
    fn a_name_key_renames_the_agent() {
        let agents = resolve(
            &map(serde_json::json!({ "original": { "name": "renamed" } })),
            &[],
        );
        assert!(names(&agents).contains(&"renamed"));
        assert!(!names(&agents).contains(&"original"));
    }

    #[test]
    fn a_name_key_is_not_swept_into_provider_options() {
        let agents = resolve(
            &map(serde_json::json!({ "original": { "name": "renamed" } })),
            &[],
        );
        let renamed = agents
            .iter()
            .find(|agent| agent.name == "renamed")
            .expect("renamed exists");
        assert!(
            !renamed.options.contains_key("name"),
            "SWEEP_EXEMPT_KEYS must keep `name` out of options"
        );
    }

    #[test]
    fn list_puts_natives_first_then_sorts_by_name() {
        let agents = list(
            &map(serde_json::json!({
                "zebra": {},
                "alpha": {},
                "plan": { "mode": "all" },
            })),
            &[],
        );
        assert_eq!(
            names(&agents),
            vec![
                "advisor",
                "build",
                "compaction",
                "deep",
                "explorer",
                "librarian",
                "looker",
                "plan",
                "summary",
                "title",
                "worker",
                "alpha",
                "zebra"
            ]
        );
    }

    #[test]
    fn the_header_is_the_line_agent_list_prints() {
        let agents = list(&OrderedMap::new(), &[]);
        let headers: Vec<String> = agents.iter().map(Agent::header).collect();
        assert_eq!(headers[0], "advisor (subagent)");
        assert!(headers.contains(&"build (primary)".to_owned()));
        assert!(headers.contains(&"deep (subagent)".to_owned()));
    }

    #[test]
    fn markdown_provenance_reaches_the_resolved_agent() {
        let paths = vec![(
            "review/security".to_owned(),
            PathBuf::from("/cfg/agent/review/security.md"),
        )];
        let agents = resolve(&map(serde_json::json!({ "review/security": {} })), &paths);
        let agent = agents
            .iter()
            .find(|agent| agent.name == "review/security")
            .expect("the markdown agent exists");
        assert_eq!(
            agent.source,
            AgentSource::Markdown {
                path: PathBuf::from("/cfg/agent/review/security.md"),
            }
        );
    }

    #[test]
    fn locale_compare_orders_lowercase_before_a_later_uppercase() {
        // A byte comparison would put uppercase `B` before lowercase `a`.
        assert_eq!(locale_compare("a", "B"), std::cmp::Ordering::Less);
        assert_eq!(locale_compare("B", "a"), std::cmp::Ordering::Greater);
        assert_eq!(locale_compare("a", "a"), std::cmp::Ordering::Equal);
    }
}

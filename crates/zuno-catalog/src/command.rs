//! Command resolution: which definition wins for a name, and what its template
//! expands to.
//!
//! Slash commands are what the user types. Four sources can define the same
//! name, so the only thing that matters here is the order in which they are
//! written into one map, and one exception to that order.
//!
//! # The precedence chain, ascending
//!
//! Oracle: `packages/opencode/src/command/index.ts:65-152`. Each level writes
//! into a single `Record<string, Info>`, so a later level *overwrites* an
//! earlier one — except the last, which does not.
//!
//! | # | level | oracle | overwrite |
//! |---|---|---|---|
//! | 1 | built-in `init`, `init-deep`, and `review` | `init`/`review`: `:70-88`; `init-deep`: Zuno native | seeds the map |
//! | 2 | `cfg.command` entries | `:90-103` | unconditional |
//! | 3 | MCP prompts | `:105-132` | unconditional |
//! | 4 | skills | `:134-152` | **only when the name is still free** (`:135`) |
//!
//! `init-deep` is a Zuno-native Level 1 command inserted between `init` and
//! `review`. It uses the same registry, precedence, expansion, and client surfaces;
//! only its repository-instruction template is new.
//!
//! Level 4's guard is the single line `if (commands[item.name]) continue`. A
//! skill therefore never shadows a built-in, a configured command, or an MCP
//! prompt, and [`Registry::build`] takes all four sources at once so a caller
//! cannot apply them out of order.
//!
//! Markdown commands need no separate level: config discovery loads them into
//! `cfg.command` before this module sees it, so they arrive as level 2.
//!
//! # Empirical confirmation
//!
//! The chain above was read from the oracle *and* observed on the real binary
//! (`opencode` 1.18.12, `GET /command`) against a fixture that collides all
//! four levels on purpose. The recorded task-15 transcript shows:
//!
//! - a `command` entry named `review` replaced the built-in's description,
//!   template, and `subtask`;
//! - a `command` entry named `srv:hello` was replaced by MCP server `srv`'s
//!   `hello` prompt (`source: "mcp"`);
//! - a skill named `collide` did not displace the `collide` command entry, and
//!   a skill named `srv:noargs` did not displace the MCP prompt — both
//!   disappeared from the listing entirely rather than appearing twice.
//!
//! # MCP prompt names carry their server
//!
//! A detail with teeth: MCP entries are not keyed by the prompt's own name.
//! `packages/opencode/src/mcp/catalog.ts:100-105` keys them
//! `sanitize(client) + ":" + sanitize(prompt)` where
//! [`sanitize`] maps every character outside `[A-Za-z0-9_-]` to `_`. So an MCP
//! prompt `hello` on server `srv` is the command `srv:hello`, and it can only
//! collide with a config command whose key is literally `srv:hello`. Level 3
//! *is* an unconditional overwrite, but in practice it only fires on that
//! colon-qualified spelling.
//!
//! # Argument expansion
//!
//! Oracle: `packages/opencode/src/session/prompt.ts:1372-1395`, with the three
//! regexes at `:1594-1596`. Expansion happens during resolution, before
//! dispatch, so a dispatched prompt is already final. [`expand`] documents each
//! rule; the surprising ones are the greedy highest placeholder, `$0`, and the
//! fact that `$ARGUMENTS` is substituted through JavaScript's replacement-pattern
//! machinery while `$1..$N` are not.
//!
//! Every rule in this module is pinned by
//! `crates/zuno-catalog/tests/command_expansion.rs`, which diffs against a
//! verbatim transcription of the oracle's own JavaScript over 59 cases.

#[cfg(test)]
mod tests;

use std::fmt;
use std::path::{Path, PathBuf};
use zuno_config::Config;
use zuno_config::discovery::{DiscoveryOptions, discover_with, merge_layers};
use zuno_config::schema::CommandConfig;
use zuno_config::schema::ordered::OrderedMap;
use zuno_error::ConfigError;
use zuno_paths::{Env, Layout};

/// The built-in `init` command's name (`command/index.ts:47`).
pub const BUILTIN_INIT: &str = "init";

/// The Zuno-native deep repository-instruction command's name.
pub const BUILTIN_INIT_DEEP: &str = "init-deep";

/// The built-in `review` command's name (`command/index.ts:48`).
pub const BUILTIN_REVIEW: &str = "review";

/// Path prefixes excluded from derived command names.
pub const COMMAND_DIRECTORY_PREFIXES: [&str; 2] = ["command/", "commands/"];

/// The built-in repository-instruction prompt.
const TEMPLATE_INITIALIZE: &str = include_str!("command/initialize.txt");

/// The Zuno-native hierarchical repository-instruction prompt.
const TEMPLATE_INITIALIZE_DEEP: &str = include_str!("command/initialize_deep.txt");

/// `command/template/review.txt`, byte-identical to the oracle's copy.
const TEMPLATE_REVIEW: &str = include_str!("command/review.txt");

/// The placeholder the built-in templates carry for the worktree
/// (`command/index.ts:75,84`).
const WORKTREE_PLACEHOLDER: &str = "${path}";

/// The whole-input placeholder (`session/prompt.ts:1390-1391`).
const ARGUMENTS_PLACEHOLDER: &str = "$ARGUMENTS";

/// Which level of the chain produced a command.
///
/// Oracle: `command/index.ts:27` — `Schema.Literals(["command", "mcp", "skill"])`.
/// Built-ins and config entries share `Command`; the oracle does not distinguish
/// them either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    /// A built-in or a `cfg.command` entry (levels 1 and 2).
    Command,
    /// An MCP prompt (level 3).
    Mcp,
    /// A skill that found its name free (level 4).
    Skill,
}

impl Source {
    /// The wire spelling the oracle uses.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Mcp => "mcp",
            Self::Skill => "skill",
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a command's prompt text comes from.
///
/// The oracle types `template` as `Promise<string> | string` (`command/index.ts:34`)
/// because MCP templates are fetched lazily. This enum is that union made
/// explicit: [`Self::Text`] is ready to expand, [`Self::Mcp`] still needs one
/// round trip to the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Template {
    /// Ready-to-expand text.
    Text(String),
    /// An MCP prompt that must be fetched first.
    Mcp(McpTemplate),
}

/// An unfetched MCP prompt, with its arguments already mapped onto positionals.
///
/// Oracle: `command/index.ts:110-129`. The indirection is deliberate — the
/// server is asked for its prompt with every declared argument bound to the
/// *literal string* `"$1"`, `"$2"`, …, so whatever text the server returns still
/// carries those placeholders, and [`expand`] then fills them with the user's
/// real arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpTemplate {
    /// The MCP client (server) name, unsanitized, as the config declared it.
    pub client: String,
    /// The prompt name, unsanitized, as the server declared it.
    pub prompt: String,
    /// `(argument name, positional)` pairs in declaration order, e.g.
    /// `[("alpha", "$1"), ("beta", "$2")]`. Empty when the prompt declares no
    /// arguments, which the oracle sends as `{}` (`command/index.ts:117-118`).
    pub arguments: Vec<(String, String)>,
}

/// One resolved command.
///
/// Field-for-field the oracle's `Command.Info` (`command/index.ts:22-32`).
/// `variant` is deliberately absent: the config entry has one
/// (`packages/core/src/v1/config/command.ts:10`) but `command/index.ts:91-102`
/// does not copy it into `Info`, so carrying it here would invent a field the
/// oracle's `/command` response does not have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    /// The name the command is invoked by.
    pub name: String,
    /// What it does, when a source supplied one.
    pub description: Option<String>,
    /// The agent to run as, config entries only.
    pub agent: Option<String>,
    /// The model to run with, config entries only.
    pub model: Option<String>,
    /// Which level produced it.
    pub source: Source,
    /// Where the prompt text comes from.
    pub template: Template,
    /// Whether to run in a subtask. `Some(true)` for the built-in `review`.
    pub subtask: Option<bool>,
    /// The placeholders the template mentions, for a picker to prompt on.
    pub hints: Vec<String>,
}

/// A skill offered as a level-4 command.
///
/// Owned by this module so that skill *discovery* (todo 14, `src/skill.rs`) and
/// command *resolution* stay decoupled: discovery maps its own record onto this
/// struct, and nothing here depends on that module's shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCommand {
    /// The skill's name, which is also the command name it would claim.
    pub name: String,
    /// The skill's description.
    pub description: Option<String>,
    /// The skill body, verbatim.
    pub content: String,
    /// Where the skill came from.
    pub location: SkillLocation,
}

/// Where a skill was found, which decides whether its template gets a base
/// directory footer.
///
/// Native embedded Skills carry no filesystem footer; file-backed Skills name the
/// parent directory that owns their resources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillLocation {
    /// A skill compiled into the binary, whose command template is its body with
    /// no filesystem footer.
    Builtin,
    /// The skill's `SKILL.md` path. The footer names its *parent* directory.
    File(PathBuf),
}

impl SkillCommand {
    /// The template the oracle builds for this skill (`command/index.ts:141-149`).
    ///
    /// A file-backed skill gets four lines joined with `\n`: the body, an empty
    /// line, the base directory, and the relative-path note. A built-in skill
    /// gets its body unchanged.
    #[must_use]
    pub fn template(&self) -> String {
        let Some(dir) = self.base_dir() else {
            return self.content.clone();
        };
        format!(
            "{}\n\nBase directory for this skill: {}\nRelative paths in this skill (e.g., scripts/, references/) are relative to this base directory.",
            self.content,
            dir.display()
        )
    }

    /// The directory the footer names, or `None` for a built-in skill.
    #[must_use]
    pub fn base_dir(&self) -> Option<&Path> {
        match &self.location {
            SkillLocation::Builtin => None,
            SkillLocation::File(path) => path.parent(),
        }
    }
}

/// One MCP prompt, as level 3 sees it.
///
/// Owned by this module for the same reason as [`SkillCommand`]: the MCP client
/// lands in todos 45-47, and resolution must be testable before it exists.
/// Shape from `packages/opencode/src/mcp/index.ts:169` — the prompt record plus
/// the client name it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpPrompt {
    /// The MCP client (server) name, unsanitized.
    pub client: String,
    /// The prompt name, unsanitized.
    pub prompt: String,
    /// The prompt's description, when the server gave one.
    pub description: Option<String>,
    /// Declared argument names in order. The oracle treats an absent list and
    /// an empty list identically (`command/index.ts:117,130`), so both are this.
    pub arguments: Vec<String>,
}

impl McpPrompt {
    /// The command name this prompt claims.
    ///
    /// Oracle: `mcp/catalog.ts:100-105` — `sanitize(client) + ":" + sanitize(name)`.
    #[must_use]
    pub fn command_name(&self) -> String {
        format!("{}:{}", sanitize(&self.client), sanitize(&self.prompt))
    }

    /// The `(name, positional)` pairs sent to the server
    /// (`command/index.ts:117-118`).
    #[must_use]
    pub fn argument_map(&self) -> Vec<(String, String)> {
        self.arguments
            .iter()
            .enumerate()
            .map(|(index, name)| (name.clone(), format!("${}", index + 1)))
            .collect()
    }

    /// The hints a picker shows (`command/index.ts:130`): one positional per
    /// declared argument, never derived from the prompt's text.
    #[must_use]
    pub fn hints(&self) -> Vec<String> {
        (1..=self.arguments.len())
            .map(|index| format!("${index}"))
            .collect()
    }
}

/// Replace every character outside `[A-Za-z0-9_-]` with `_`.
///
/// Oracle: `mcp/catalog.ts:113` — `value.replace(/[^a-zA-Z0-9_-]/g, "_")`. The
/// character class is ASCII-only, so a non-ASCII character is replaced whole.
#[must_use]
pub fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Discover configured and Markdown-defined commands for one working directory.
///
/// # Errors
///
/// Returns configuration, filesystem, or Markdown schema failures.
pub fn load_map(
    directory: &Path,
    worktree: Option<&Path>,
    env: &Env,
) -> Result<OrderedMap<CommandConfig>, ConfigError> {
    let options = DiscoveryOptions::new(directory, worktree, env.clone());
    let config = discover_with(&options)?;
    let mut commands = config.command.unwrap_or_default();

    let layout = Layout::resolve(env);
    for dir in layout.config_directories(directory, worktree) {
        for root_name in ["command", "commands"] {
            let root = dir.join(root_name);
            if !root.is_dir() {
                continue;
            }
            for file in crate::agent::markdown_files(&root)? {
                let Some((name, command)) = read_markdown_command(&dir, &file)? else {
                    continue;
                };
                let mut overlay = OrderedMap::new();
                overlay.insert(name, command);
                commands = merge_command_maps(&commands, &overlay)?;
            }
        }
    }

    if let Some(text) = env.truthy_value("ZUNO_CONFIG_CONTENT")
        && let Ok(layer) = serde_json::from_str::<Config>(text)
        && let Some(from_env) = layer.command
    {
        commands = merge_command_maps(&commands, &from_env)?;
    }

    Ok(commands)
}

fn read_markdown_command(
    dir: &Path,
    file: &Path,
) -> Result<Option<(String, CommandConfig)>, ConfigError> {
    let text = std::fs::read_to_string(file).map_err(|source| ConfigError::Io {
        path: file.to_path_buf(),
        source,
    })?;
    let Ok(document) = crate::agent::frontmatter::parse(&text) else {
        tracing::warn!(
            path = %file.display(),
            "skipping command: its frontmatter could not be parsed"
        );
        return Ok(None);
    };

    let relative = crate::agent::relative_path(dir, file);
    let derived = crate::agent::entry_name_from_path(&relative, &COMMAND_DIRECTORY_PREFIXES);
    let mut object = document.data;
    let name = object
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map_or(derived, str::to_owned);
    object.insert(
        "template".to_owned(),
        serde_json::Value::String(document.content.trim().to_owned()),
    );
    let command = serde_json::from_value(serde_json::Value::Object(object)).map_err(|source| {
        ConfigError::Json {
            path: file.to_path_buf(),
            source,
        }
    })?;
    Ok(Some((name, command)))
}

/// Deep-merge `overlay` over `base` using configuration merge semantics.
///
/// # Errors
///
/// Returns an error if either map cannot be represented as configuration JSON.
pub fn merge_command_maps(
    base: &OrderedMap<CommandConfig>,
    overlay: &OrderedMap<CommandConfig>,
) -> Result<OrderedMap<CommandConfig>, ConfigError> {
    let merged = merge_layers([
        Config {
            command: Some(base.clone()),
            ..Config::default()
        },
        Config {
            command: Some(overlay.clone()),
            ..Config::default()
        },
    ])?;
    Ok(merged.command.unwrap_or_default())
}

/// The four sources of commands, gathered so precedence cannot be misapplied.
///
/// [`Registry::build`] consumes this whole struct. There is no per-level entry
/// point on purpose: the ordering *is* the feature, so it lives in one place.
#[derive(Debug, Clone, Copy)]
pub struct Sources<'a> {
    /// The worktree path the built-in templates interpolate
    /// (`command/index.ts:75,84`).
    pub worktree: &'a str,
    /// `cfg.command`, already including markdown commands that config discovery
    /// loaded into it.
    pub config: Option<&'a OrderedMap<CommandConfig>>,
    /// Prompts from every connected MCP server.
    pub mcp_prompts: &'a [McpPrompt],
    /// Every discovered skill.
    pub skills: &'a [SkillCommand],
}

impl<'a> Sources<'a> {
    /// Just the built-ins, for a caller with nothing else to contribute.
    #[must_use]
    pub const fn new(worktree: &'a str) -> Self {
        Self {
            worktree,
            config: None,
            mcp_prompts: &[],
            skills: &[],
        }
    }

    /// Attach `cfg.command`.
    #[must_use]
    pub const fn with_config(mut self, config: Option<&'a OrderedMap<CommandConfig>>) -> Self {
        self.config = config;
        self
    }

    /// Attach MCP prompts.
    #[must_use]
    pub const fn with_mcp_prompts(mut self, prompts: &'a [McpPrompt]) -> Self {
        self.mcp_prompts = prompts;
        self
    }

    /// Attach skills.
    #[must_use]
    pub const fn with_skills(mut self, skills: &'a [SkillCommand]) -> Self {
        self.skills = skills;
        self
    }
}

/// Every command available to a session, in the order a picker lists them.
///
/// Insertion order matches the oracle's, including the detail that overwriting a
/// name keeps the position the name first appeared at — a JavaScript object
/// property that [`OrderedMap::insert`] reproduces. Zuno inserts `init-deep`
/// between `init` and `review`, so a `review` config entry stays in slot 2 where
/// the built-in put it.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    commands: OrderedMap<Info>,
}

impl Registry {
    /// Resolve every source into one map, ascending through the four levels.
    #[must_use]
    pub fn build(sources: &Sources<'_>) -> Self {
        let mut commands: OrderedMap<Info> = OrderedMap::new();

        // Level 1 — built-ins. Zuno's native `init-deep` sits between the
        // upstream-shaped `init` and `review` entries.
        commands.insert(BUILTIN_INIT, builtin_init(sources.worktree));
        commands.insert(BUILTIN_INIT_DEEP, builtin_init_deep(sources.worktree));
        commands.insert(BUILTIN_REVIEW, builtin_review(sources.worktree));

        // Level 2 — config commands, unconditional (`:90-103`).
        if let Some(config) = sources.config {
            for (name, entry) in config.iter() {
                commands.insert(name, config_command(name, entry));
            }
        }

        // Level 3 — MCP prompts, unconditional (`:105-132`).
        for prompt in sources.mcp_prompts {
            let name = prompt.command_name();
            commands.insert(name.clone(), mcp_command(&name, prompt));
        }

        // Level 4 — skills, and only into a free name (`:134-152`, guard at `:135`).
        for skill in sources.skills {
            if commands.contains_key(&skill.name) {
                continue;
            }
            commands.insert(skill.name.clone(), skill_command(skill));
        }

        Self { commands }
    }

    /// The command registered under `name` (`command/index.ts:161-164`).
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Info> {
        self.commands.get(name)
    }

    /// Every command in listing order (`command/index.ts:166-169`).
    pub fn list(&self) -> impl Iterator<Item = &Info> {
        self.commands.iter().map(|(_, info)| info)
    }

    /// Every registered name, in listing order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.commands.keys()
    }

    /// How many commands are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    /// Whether nothing is registered. Never true in practice — the built-ins
    /// always seed the map — but `clippy::len_without_is_empty` asks.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// Resolve `name` against `arguments`, expanding the template now.
    ///
    /// Expansion happens here rather than after dispatch, matching the oracle's
    /// `SessionPrompt.command` (`session/prompt.ts:1362-1395`), which resolves,
    /// awaits the template, and expands before it builds a single message.
    ///
    /// An MCP command cannot finish in one step: it returns
    /// [`Resolution::PendingMcp`], and the caller completes it with
    /// [`PendingMcp::complete`] once the server has answered.
    pub fn resolve(&self, name: &str, arguments: &str) -> Result<Resolution, CommandError> {
        let Some(info) = self.commands.get(name) else {
            return Err(CommandError::NotFound {
                name: name.to_owned(),
                available: self.names().map(str::to_owned).collect(),
            });
        };
        Ok(match &info.template {
            Template::Text(template) => Resolution::Ready(Resolved {
                name: info.name.clone(),
                source: info.source,
                agent: info.agent.clone(),
                model: info.model.clone(),
                subtask: info.subtask,
                prompt: expand(template, arguments),
            }),
            Template::Mcp(mcp) => Resolution::PendingMcp(PendingMcp {
                name: info.name.clone(),
                source: info.source,
                agent: info.agent.clone(),
                model: info.model.clone(),
                subtask: info.subtask,
                request: mcp.clone(),
                arguments: arguments.to_owned(),
            }),
        })
    }
}

/// The outcome of [`Registry::resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// The prompt is final.
    Ready(Resolved),
    /// The MCP server must be asked first.
    PendingMcp(PendingMcp),
}

/// A command with its prompt already expanded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The command's name.
    pub name: String,
    /// Which level defined it.
    pub source: Source,
    /// The agent to run as, if the command pinned one.
    pub agent: Option<String>,
    /// The model to run with, if the command pinned one.
    pub model: Option<String>,
    /// Whether to run in a subtask.
    pub subtask: Option<bool>,
    /// The final prompt text.
    pub prompt: String,
}

/// An MCP command waiting on its server.
///
/// The user's arguments are carried through unexpanded because expansion runs on
/// the *server's* answer: `session/prompt.ts:1374` awaits `cmd.template` and
/// only then applies the placeholder pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMcp {
    /// The command's name, already colon-qualified.
    pub name: String,
    /// Always [`Source::Mcp`]; kept so [`Resolved`] can be built uniformly.
    pub source: Source,
    /// The agent to run as. Never set by an MCP prompt, kept for uniformity.
    pub agent: Option<String>,
    /// The model to run with. Never set by an MCP prompt.
    pub model: Option<String>,
    /// Whether to run in a subtask. Never set by an MCP prompt.
    pub subtask: Option<bool>,
    /// What to ask the server for, arguments already bound to positionals.
    pub request: McpTemplate,
    /// The user's raw argument string, expanded once the answer arrives.
    pub arguments: String,
}

impl PendingMcp {
    /// Finish resolution with the server's `prompts/get` messages.
    ///
    /// `messages` is one entry per returned message: `Some(text)` for a text
    /// content block, `None` for any other kind. Oracle: `command/index.ts:121-126`
    /// — non-text becomes an empty string but is still joined, and an absent or
    /// wholly empty result becomes `""`.
    #[must_use]
    pub fn complete(self, messages: &[Option<String>]) -> Resolved {
        Resolved {
            name: self.name,
            source: self.source,
            agent: self.agent,
            model: self.model,
            subtask: self.subtask,
            prompt: expand(&join_prompt_messages(messages), &self.arguments),
        }
    }
}

/// Flatten an MCP `prompts/get` result into one template string.
///
/// Oracle: `command/index.ts:121-126`.
#[must_use]
pub fn join_prompt_messages(messages: &[Option<String>]) -> String {
    messages
        .iter()
        .map(|message| message.as_deref().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Resolution failed.
///
/// One variant, because resolution has exactly one failure mode. Expansion has
/// none: every malformed template reference produces text, never an error and
/// never a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandError {
    /// No source defined this name. `available` lists what did resolve, so the
    /// message can name the alternatives the way the oracle's does
    /// (`session/prompt.ts:1364-1366`).
    NotFound {
        /// The name that was asked for.
        name: String,
        /// Every registered name, in listing order.
        available: Vec<String>,
    },
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { name, available } => {
                write!(f, "Command not found: \"{name}\".")?;
                if !available.is_empty() {
                    write!(f, " Available commands: {}", available.join(", "))?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for CommandError {}

fn builtin_init(worktree: &str) -> Info {
    Info {
        name: BUILTIN_INIT.to_owned(),
        description: Some("guided AGENTS.md setup".to_owned()),
        agent: None,
        model: None,
        source: Source::Command,
        template: Template::Text(interpolate_worktree(TEMPLATE_INITIALIZE, worktree)),
        subtask: None,
        hints: hints(TEMPLATE_INITIALIZE),
    }
}

fn builtin_init_deep(worktree: &str) -> Info {
    Info {
        name: BUILTIN_INIT_DEEP.to_owned(),
        description: Some("deep AGENTS.md setup [--create-new] [--max-depth=N]".to_owned()),
        agent: None,
        model: None,
        source: Source::Command,
        template: Template::Text(interpolate_worktree(TEMPLATE_INITIALIZE_DEEP, worktree)),
        subtask: None,
        hints: hints(TEMPLATE_INITIALIZE_DEEP),
    }
}

fn builtin_review(worktree: &str) -> Info {
    Info {
        name: BUILTIN_REVIEW.to_owned(),
        description: Some("review changes [commit|branch|pr], defaults to uncommitted".to_owned()),
        agent: None,
        model: None,
        source: Source::Command,
        template: Template::Text(interpolate_worktree(TEMPLATE_REVIEW, worktree)),
        subtask: Some(true),
        hints: hints(TEMPLATE_REVIEW),
    }
}

/// Substitute the worktree into a built-in template.
///
/// The oracle uses `String.prototype.replace` with a *string* pattern
/// (`command/index.ts:75,84`), which replaces only the first occurrence.
/// Both initialization templates have exactly one; `review.txt` has none,
/// making its substitution a no-op.
fn interpolate_worktree(template: &str, worktree: &str) -> String {
    template.replacen(WORKTREE_PLACEHOLDER, worktree, 1)
}

fn config_command(name: &str, entry: &CommandConfig) -> Info {
    Info {
        name: name.to_owned(),
        description: entry.description.clone(),
        agent: entry.agent.clone(),
        model: entry.model.clone(),
        source: Source::Command,
        template: Template::Text(entry.template.clone()),
        subtask: entry.subtask,
        hints: hints(&entry.template),
    }
}

fn mcp_command(name: &str, prompt: &McpPrompt) -> Info {
    Info {
        name: name.to_owned(),
        description: prompt.description.clone(),
        agent: None,
        model: None,
        source: Source::Mcp,
        template: Template::Mcp(McpTemplate {
            client: prompt.client.clone(),
            prompt: prompt.prompt.clone(),
            arguments: prompt.argument_map(),
        }),
        subtask: None,
        hints: prompt.hints(),
    }
}

fn skill_command(skill: &SkillCommand) -> Info {
    Info {
        name: skill.name.clone(),
        description: skill.description.clone(),
        agent: None,
        model: None,
        source: Source::Skill,
        template: Template::Text(skill.template()),
        subtask: None,
        // Oracle: `command/index.ts:150` — always empty for a skill, even when
        // the body happens to contain `$1`.
        hints: Vec::new(),
    }
}

/// One `$<digits>` occurrence found in a template.
#[derive(Debug, Clone, Copy)]
struct Placeholder<'a> {
    /// Byte offset of the `$`.
    start: usize,
    /// Byte offset one past the last digit.
    end: usize,
    /// The whole match, `$` included, e.g. `$01`.
    raw: &'a str,
    /// The digits as a number, saturated. Saturation is unobservable: any
    /// position past the argument count expands to empty regardless.
    position: u64,
}

/// Find every `$<digits>` occurrence, in order.
///
/// Oracle: `session/prompt.ts:1595` — `/\$(\d+)/g`. `\d` is ASCII-only in
/// JavaScript, so `$٣` is not a placeholder; confirmed against the oracle.
fn scan_placeholders(template: &str) -> Vec<Placeholder<'_>> {
    let bytes = template.as_bytes();
    let mut found = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'$' {
            let mut end = index + 1;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end > index + 1 {
                let raw = &template[index..end];
                found.push(Placeholder {
                    start: index,
                    end,
                    raw,
                    position: raw[1..].parse::<u64>().unwrap_or(u64::MAX),
                });
                index = end;
                continue;
            }
        }
        index += 1;
    }
    found
}

/// The placeholders a template mentions, for a picker to prompt on.
///
/// Oracle: `command/index.ts:36-43`. Numbered placeholders are deduplicated
/// (insertion-ordered `Set`) then sorted *lexicographically*, not numerically —
/// so `$10` precedes `$2`, confirmed on the real binary. `$ARGUMENTS` is
/// appended last when present. The raw spelling survives: `$01` stays `$01`.
#[must_use]
pub fn hints(template: &str) -> Vec<String> {
    let mut numbered: Vec<String> = Vec::new();
    for placeholder in scan_placeholders(template) {
        let raw = placeholder.raw.to_owned();
        if !numbered.contains(&raw) {
            numbered.push(raw);
        }
    }
    numbered.sort();
    if template.contains(ARGUMENTS_PLACEHOLDER) {
        numbered.push(ARGUMENTS_PLACEHOLDER.to_owned());
    }
    numbered
}

/// Split the user's raw argument string into positional arguments.
///
/// Oracle: `session/prompt.ts:1372-1373,1594,1596` — one global match of
/// `/(?:\[Image\s+\d+\]|"[^"]*"|'[^']*'|[^\s"']+)/gi`, then `/^["']|["']$/g`
/// stripped from each token. Consequences worth knowing, all confirmed against
/// the oracle:
///
/// - `[Image 3]` is one token, case-insensitively.
/// - A quoted run is one token and loses its quotes, so `""` yields an empty
///   token.
/// - An *unpaired* quote matches nothing and is skipped entirely: `" second`
///   yields just `["second"]`.
/// - `don't` splits into `don` and `t`, because the apostrophe opens a group
///   that never closes.
#[must_use]
pub fn tokenize(arguments: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut rest = arguments;
    while !rest.is_empty() {
        if let Some(length) = match_token(rest) {
            tokens.push(trim_quotes(&rest[..length]).to_owned());
            rest = &rest[length..];
        } else {
            let step = rest
                .chars()
                .next()
                .map_or_else(|| rest.len(), char::len_utf8);
            rest = &rest[step..];
        }
    }
    tokens
}

/// The byte length of the token starting at position 0, if one matches there.
///
/// Alternatives are tried in the regex's order, which matters: `[Image 3]`
/// outranks the bare-run alternative that would otherwise stop at the space.
fn match_token(rest: &str) -> Option<usize> {
    match_image_token(rest)
        .or_else(|| match_quoted_token(rest, '"'))
        .or_else(|| match_quoted_token(rest, '\''))
        .or_else(|| match_bare_token(rest))
}

/// `\[Image\s+\d+\]`, case-insensitive.
fn match_image_token(rest: &str) -> Option<usize> {
    let bytes = rest.as_bytes();
    if bytes.first() != Some(&b'[') {
        return None;
    }
    let after_word = 1 + "image".len();
    if rest.len() < after_word || !rest[1..after_word].eq_ignore_ascii_case("image") {
        return None;
    }
    let mut index = after_word;
    let spaces_start = index;
    // `\s+` — JavaScript's `\s` is close enough to Unicode White_Space that only
    // U+FEFF differs, which cannot appear in a rendered `[Image N]` marker.
    while let Some(c) = rest[index..].chars().next() {
        if c.is_whitespace() {
            index += c.len_utf8();
        } else {
            break;
        }
    }
    if index == spaces_start {
        return None;
    }
    let digits_start = index;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    if index == digits_start {
        return None;
    }
    if bytes.get(index) != Some(&b']') {
        return None;
    }
    Some(index + 1)
}

/// `"[^"]*"` or `'[^']*'`.
fn match_quoted_token(rest: &str, quote: char) -> Option<usize> {
    let mut chars = rest.char_indices();
    if chars.next()?.1 != quote {
        return None;
    }
    for (offset, c) in chars {
        if c == quote {
            return Some(offset + c.len_utf8());
        }
    }
    None
}

/// `[^\s"']+`.
fn match_bare_token(rest: &str) -> Option<usize> {
    let mut length = 0;
    for (offset, c) in rest.char_indices() {
        if c.is_whitespace() || c == '"' || c == '\'' {
            break;
        }
        length = offset + c.len_utf8();
    }
    (length > 0).then_some(length)
}

/// Strip one leading and one trailing quote.
///
/// Oracle: `session/prompt.ts:1596` — `/^["']|["']$/g`, a *global* replace of an
/// anchored alternation, which strips at most one at each end. The trailing
/// strip cannot reach back to a character the leading strip already consumed,
/// so a lone `"` survives as an empty string rather than underflowing.
fn trim_quotes(token: &str) -> &str {
    let mut out = token;
    if let Some(first) = out.chars().next()
        && (first == '"' || first == '\'')
    {
        out = &out[first.len_utf8()..];
    }
    if let Some(last) = out.chars().next_back()
        && (last == '"' || last == '\'')
    {
        out = &out[..out.len() - last.len_utf8()];
    }
    out
}

/// Substitute the user's arguments into a template.
///
/// Oracle: `session/prompt.ts:1372-1395`, in five steps.
///
/// 1. **Tokenize** the raw input ([`tokenize`]).
/// 2. **Find** every `$<digits>` and take `last` = the highest number, or `0`
///    when there are none.
/// 3. **Replace** each occurrence:
///    - past the end of the argument list → the empty string, which is why `$3`
///      with two arguments vanishes rather than erroring;
///    - equal to `last` → *every remaining* argument joined by one space, so
///      the highest placeholder in a template is greedy. `A=[$1] B=[$2]` with
///      four arguments puts three of them in `B`;
///    - otherwise → that one argument;
///    - `$0` is a JavaScript artefact: `args[-1]` is `undefined`, so it renders
///      the literal text `undefined` — unless `$0` is itself the highest
///      placeholder, when `slice(-1)` makes it the *last* argument.
/// 4. **Replace `$ARGUMENTS`** with the raw input — quotes, runs of spaces and
///    newlines intact, since this is the untokenized string. This step goes
///    through JavaScript's replacement-pattern machinery, so `$$`, `$&`,
///    `` $` `` and `$'` *inside the user's arguments* are interpreted
///    ([`js_replace_all`]). Step 3 uses a function replacer and does not.
/// 5. **Append** the raw input after a blank line when the template mentions no
///    placeholder at all and the input is not blank, then trim.
///
/// Never fails and never panics: a template referencing `$999`, `$0`, a bare
/// `$`, or `$5.00` all produce text.
#[must_use]
pub fn expand(template: &str, arguments: &str) -> String {
    let args = tokenize(arguments);
    let placeholders = scan_placeholders(template);
    let last = placeholders
        .iter()
        .map(|placeholder| placeholder.position)
        .max()
        .unwrap_or(0);

    let mut substituted = String::with_capacity(template.len());
    let mut cursor = 0;
    for placeholder in &placeholders {
        substituted.push_str(&template[cursor..placeholder.start]);
        substituted.push_str(&substitute_positional(placeholder.position, last, &args));
        cursor = placeholder.end;
    }
    substituted.push_str(&template[cursor..]);

    let uses_arguments = template.contains(ARGUMENTS_PLACEHOLDER);
    let mut expanded = js_replace_all(&substituted, ARGUMENTS_PLACEHOLDER, arguments);
    if placeholders.is_empty() && !uses_arguments && !arguments.trim().is_empty() {
        expanded.push_str("\n\n");
        expanded.push_str(arguments);
    }
    expanded.trim().to_owned()
}

/// One placeholder's replacement text (`session/prompt.ts:1383-1389`).
fn substitute_positional(position: u64, last: u64, args: &[String]) -> String {
    if position == 0 {
        // `argIndex` is -1, so the length guard never fires.
        if position == last {
            // `args.slice(-1)` — the final argument, or nothing.
            return args.last().cloned().unwrap_or_default();
        }
        // `args[-1]` is `undefined`, which the replacer stringifies.
        return "undefined".to_owned();
    }
    let index = position - 1;
    if index >= args.len() as u64 {
        return String::new();
    }
    let Ok(index) = usize::try_from(index) else {
        return String::new();
    };
    if position == last {
        return args[index..].join(" ");
    }
    args[index].clone()
}

/// `String.prototype.replaceAll` with a string search *and* a string
/// replacement, including the `$` substitution patterns that form implies.
///
/// The oracle reaches this at `session/prompt.ts:1391`. Because the replacement
/// is a plain string rather than a function, ECMA-262 `GetSubstitution` runs
/// over it: `$$` becomes `$`, `$&` becomes the matched text, `` $` `` becomes
/// everything before the match, and `$'` becomes everything after it. There are
/// no capture groups, so `$1` and `$<name>` are left alone. Every one of these
/// was confirmed against the oracle's own JavaScript rather than inferred.
#[must_use]
pub fn js_replace_all(haystack: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return haystack.to_owned();
    }
    let mut out = String::with_capacity(haystack.len());
    let mut cursor = 0;
    while let Some(offset) = haystack[cursor..].find(needle) {
        let start = cursor + offset;
        let end = start + needle.len();
        out.push_str(&haystack[cursor..start]);
        expand_substitution(
            &mut out,
            replacement,
            needle,
            &haystack[..start],
            &haystack[end..],
        );
        cursor = end;
    }
    out.push_str(&haystack[cursor..]);
    out
}

/// ECMA-262 `GetSubstitution` for a match with no capture groups.
fn expand_substitution(
    out: &mut String,
    replacement: &str,
    matched: &str,
    before: &str,
    after: &str,
) {
    let bytes = replacement.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'$' {
            let c = replacement[index..]
                .chars()
                .next()
                .unwrap_or_else(|| unreachable!("index is on a char boundary"));
            out.push(c);
            index += c.len_utf8();
            continue;
        }
        match bytes.get(index + 1) {
            Some(b'$') => {
                out.push('$');
                index += 2;
            }
            Some(b'&') => {
                out.push_str(matched);
                index += 2;
            }
            Some(b'`') => {
                out.push_str(before);
                index += 2;
            }
            Some(b'\'') => {
                out.push_str(after);
                index += 2;
            }
            // No capture groups, so `$1`, `$<name>`, and a trailing `$` are all
            // literal.
            _ => {
                out.push('$');
                index += 1;
            }
        }
    }
}

//! Slash-command routing shared by the prompt and its autocomplete popup.
//!
//! There are deliberately two command classes. UI commands resolve to an action
//! from [`crate::keybind::DEFINITIONS`] and never enter the model prompt. Catalog
//! commands are plain metadata supplied by the host; the host owns template
//! expansion because this leaf crate must not depend on `zuno-catalog`.
//!
//! # Names and precedence
//!
//! UI names and descriptions are derived from [`DEFINITIONS`] rather than copied
//! into autocomplete. A resource action ending in `_list` uses the short natural
//! word as its canonical spelling and accepts the plural too (`model_list` ->
//! `/model`, alias `/models`). The other actions use their semantic segment
//! (`session_new` -> `/new`, `diff_open` -> `/diff`). Only compatibility aliases
//! that cannot be derived are listed below.
//!
//! UI commands win a same-name collision with a catalog command. This keeps a
//! local action such as `/help` from unexpectedly becoming model input after a
//! configuration change. The colliding catalog command remains available to the
//! headless command surface, but is intentionally hidden from this slash surface.
//!
//! A doubled leading slash is the literal escape hatch: `//review this` submits
//! `/review this` as ordinary prompt text. Its cost is that a prompt that really
//! starts with two slashes must be written with three.

use std::collections::BTreeSet;

use zuno_engine::session_command::SessionCommand;

use crate::keybind::DEFINITIONS;

#[cfg(test)]
#[path = "slash_tests.rs"]
mod tests;

/// Catalog metadata projected by the runtime host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogCommand {
    /// Invocation name without the leading slash.
    pub name: String,
    /// Human-readable summary shown by autocomplete.
    pub description: Option<String>,
    /// Whether this name expands a command template or selects one exact Skill.
    pub kind: CatalogCommandKind,
}

impl CatalogCommand {
    /// Construct host-neutral command metadata.
    #[must_use]
    pub fn new(name: impl Into<String>, description: Option<String>) -> Self {
        Self {
            name: name.into(),
            description,
            kind: CatalogCommandKind::Command,
        }
    }

    /// Construct a direct Skill command whose source identity must survive dispatch.
    #[must_use]
    pub fn skill(
        name: impl Into<String>,
        description: Option<String>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description,
            kind: CatalogCommandKind::Skill {
                source: source.into(),
            },
        }
    }
}

/// Host-neutral catalog command semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogCommandKind {
    /// Resolve a configured, built-in, or MCP command template.
    Command,
    /// Select and load one exact Skill source before any provider request.
    Skill {
        /// Absolute `SKILL.md` path or native source locator.
        source: String,
    },
}

/// What selecting or submitting a slash command does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommandKind {
    /// Dispatch a keybind action inside the TUI.
    UiAction(&'static str),
    /// Ask the host to resolve a catalog template.
    Catalog,
    /// Ask the host to load one exact Skill before optional user arguments.
    Skill {
        /// Exact Skill source locator.
        source: String,
    },
    /// Ask the runtime host to perform a session-local control operation.
    Host(HostCommand),
}

/// A slash command executed by the runtime host rather than the model or view.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HostCommand {
    /// Summarize older durable history and keep the recent tail verbatim.
    Compact,
    /// Restore the worktree boundary before the most recent completed turn.
    Undo,
    /// Reapply the most recently undone turn boundary.
    Redo,
    /// Inspect or mutate the durable top-level goal for this session.
    Goal(String),
    /// Select a configured model-team preset, or open the picker when omitted.
    Preset(Option<String>),
    /// Launch a native Council preset, or open the picker when omitted.
    Council(String),
    /// Interactively enter or leave Plan mode.
    Plan,
    /// Enter Plan mode without another prompt.
    StartPlan,
    /// Confirm the durable plan and switch to implementation.
    StartWork,
    /// Stop one background execution, or open the selector when omitted.
    Stop(Option<String>),
}

/// One discoverable slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    /// Canonical display and invocation name, without `/`.
    pub name: String,
    /// Additional accepted names, without `/`.
    pub aliases: Vec<String>,
    /// Description from the keybind definition or catalog metadata.
    pub description: String,
    /// Route selected by this command.
    pub kind: SlashCommandKind,
}

impl SlashCommand {
    fn matches(&self, name: &str) -> bool {
        self.name == name || self.aliases.iter().any(|alias| alias == name)
    }
}

/// The result of classifying submitted editor text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashSubmission {
    /// Ordinary model input, including text escaped with `//`.
    Prompt(String),
    /// An in-process UI action. It must not be forwarded to the model.
    UiAction(&'static str),
    /// A catalog invocation for the host to resolve.
    Catalog {
        /// Catalog command name without `/`.
        command: String,
        /// Unexpanded argument tail.
        arguments: String,
    },
    /// A direct Skill invocation preserving the discovered source identity.
    Skill {
        /// Skill name without `/`.
        name: String,
        /// Exact source selected by discovery.
        source: String,
        /// Optional task text following the slash name.
        arguments: String,
    },
    /// A session-local operation for the runtime host.
    Host(HostCommand),
    /// A slash-prefixed name owned by neither command class.
    Unknown(String),
}

/// Merged slash surface, with UI commands ahead of host catalog commands.
#[derive(Debug, Clone)]
pub struct SlashRouter {
    commands: Vec<SlashCommand>,
}

impl SlashRouter {
    /// Build the merged surface. Catalog entries colliding with a UI canonical
    /// name or alias are omitted because UI actions have precedence.
    #[must_use]
    pub fn new(catalog: impl IntoIterator<Item = CatalogCommand>) -> Self {
        let mut commands = ui_commands();
        let ui_names = commands
            .iter()
            .flat_map(|command| {
                std::iter::once(command.name.as_str())
                    .chain(command.aliases.iter().map(String::as_str))
            })
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();

        // Classification happens while the two registries are projected, not after a
        // slash-name list has been built. That distinction is what makes the boundary
        // survive new upstream entries: a future `workspace_recover` or `session_reshare`
        // is classified with its family automatically rather than waiting for somebody to
        // append its spelling to a deny-list.
        let mut catalog = catalog
            .into_iter()
            .filter(|command| {
                matches!(command.kind, CatalogCommandKind::Skill { .. })
                    || command_family(&command.name) == CommandFamily::Local
            })
            .collect::<Vec<_>>();
        catalog.sort_by(|left, right| left.name.cmp(&right.name));
        catalog.dedup_by(|left, right| left.name == right.name);
        commands.extend(
            catalog
                .into_iter()
                .filter(|command| !command.name.is_empty() && !ui_names.contains(&command.name))
                .map(|command| SlashCommand {
                    name: command.name,
                    aliases: Vec::new(),
                    description: command
                        .description
                        .unwrap_or_else(|| "Run catalog command".to_owned()),
                    kind: match command.kind {
                        CatalogCommandKind::Command => SlashCommandKind::Catalog,
                        CatalogCommandKind::Skill { source } => SlashCommandKind::Skill { source },
                    },
                }),
        );
        Self { commands }
    }

    /// Commands shown by autocomplete, in stable UI-then-catalog order.
    #[must_use]
    pub fn commands(&self) -> &[SlashCommand] {
        &self.commands
    }

    /// Classify editor text without expanding catalog templates.
    #[must_use]
    pub fn resolve(&self, input: &str) -> SlashSubmission {
        if let Some(literal) = input.strip_prefix("//") {
            return SlashSubmission::Prompt(format!("/{literal}"));
        }
        let Some(body) = input.strip_prefix('/') else {
            return SlashSubmission::Prompt(input.to_owned());
        };
        let split = body.find(char::is_whitespace).unwrap_or(body.len());
        let name = &body[..split];
        let arguments = body[split..].trim().to_owned();
        let Some(command) = self.commands.iter().find(|command| command.matches(name)) else {
            return SlashSubmission::Unknown(name.to_owned());
        };
        match &command.kind {
            SlashCommandKind::UiAction(action) => SlashSubmission::UiAction(action),
            SlashCommandKind::Catalog => SlashSubmission::Catalog {
                command: command.name.clone(),
                arguments,
            },
            SlashCommandKind::Skill { source } => SlashSubmission::Skill {
                name: command.name.clone(),
                source: source.clone(),
                arguments,
            },
            SlashCommandKind::Host(HostCommand::Stop(_)) => SlashSubmission::Host(
                HostCommand::Stop((!arguments.is_empty()).then_some(arguments)),
            ),
            SlashCommandKind::Host(HostCommand::Goal(_)) => {
                SlashSubmission::Host(HostCommand::Goal(arguments))
            }
            SlashCommandKind::Host(HostCommand::Preset(_)) => SlashSubmission::Host(
                HostCommand::Preset((!arguments.is_empty()).then_some(arguments)),
            ),
            SlashCommandKind::Host(HostCommand::Council(_)) => {
                SlashSubmission::Host(HostCommand::Council(arguments))
            }
            SlashCommandKind::Host(command) => SlashSubmission::Host(command.clone()),
        }
    }
}

impl Default for SlashRouter {
    fn default() -> Self {
        Self::new([])
    }
}

#[derive(Debug, Clone, Copy)]
struct UiSpec {
    action: &'static str,
    aliases: &'static [&'static str],
}

const UI_SPECS: &[UiSpec] = &[
    UiSpec {
        action: "model_list",
        aliases: &["mo"],
    },
    UiSpec {
        action: "agent_list",
        aliases: &[],
    },
    UiSpec {
        action: "mcp_list",
        aliases: &[],
    },
    UiSpec {
        action: "status_view",
        aliases: &[],
    },
    UiSpec {
        action: "debug_view",
        aliases: &[],
    },
    UiSpec {
        action: "prompt_skills",
        aliases: &["skill"],
    },
    UiSpec {
        action: "session_list",
        aliases: &["resume", "continue"],
    },
    UiSpec {
        action: "session_new",
        aliases: &[],
    },
    UiSpec {
        action: "diff_open",
        aliases: &[],
    },
    UiSpec {
        action: "theme_list",
        aliases: &[],
    },
    UiSpec {
        action: "help_show",
        aliases: &[],
    },
    UiSpec {
        action: "command_list",
        aliases: &[],
    },
    UiSpec {
        action: "editor_open",
        aliases: &[],
    },
    UiSpec {
        action: "display_thinking",
        aliases: &["toggle-thinking"],
    },
    UiSpec {
        action: "app_exit",
        aliases: &["quit", "q"],
    },
    UiSpec {
        action: "ps_view",
        aliases: &[],
    },
    UiSpec {
        action: "memory_view",
        aliases: &[],
    },
];

fn ui_commands() -> Vec<SlashCommand> {
    // This route metadata contains only actions `SessionScreen` consumes. Definitions for
    // planned surfaces remain in the source binding table, but advertising one before its
    // screen arm exists turns a valid slash command into silent failure. `variant_list` is
    // the concrete case: the complete `variant` scope is `variant_cycle` on `ctrl+t` plus
    // unbound `variant_list`, so registering it would not steal a bare character the way
    // `diff` can, yet `/variant` would still dispatch into silence because there is no
    // variant picker. The route stays absent until there is a consumer.
    DEFINITIONS
        .iter()
        .chain(crate::keybind::LOCAL_DEFINITIONS.iter())
        .filter(|definition| {
            command_family(definition.name) == CommandFamily::Local
                && command_family(definition.command) == CommandFamily::Local
        })
        .filter_map(|definition| {
            let spec = UI_SPECS
                .iter()
                .find(|spec| spec.action == definition.name)?;
            let name = canonical_name(spec.action);
            let mut aliases = spec
                .aliases
                .iter()
                .map(|alias| (*alias).to_owned())
                .collect::<Vec<_>>();
            if let Some(stem) = spec.action.strip_suffix("_list") {
                let singular = stem.replace('_', "-");
                let plural = format!("{singular}s");
                if singular != name {
                    aliases.push(singular);
                }
                if plural != name {
                    aliases.push(plural);
                }
            }
            aliases.sort();
            aliases.dedup();
            Some(SlashCommand {
                name,
                aliases,
                description: definition.description.to_owned(),
                kind: SlashCommandKind::UiAction(definition.name),
            })
        })
        .chain([
            SlashCommand {
                name: SessionCommand::Compact.name().to_owned(),
                aliases: Vec::new(),
                description: SessionCommand::Compact.description().to_owned(),
                kind: SlashCommandKind::Host(HostCommand::Compact),
            },
            SlashCommand {
                name: "undo".to_owned(),
                aliases: Vec::new(),
                description: "Restore the worktree before the last completed turn".to_owned(),
                kind: SlashCommandKind::Host(HostCommand::Undo),
            },
            SlashCommand {
                name: "redo".to_owned(),
                aliases: Vec::new(),
                description: "Reapply the most recently undone turn".to_owned(),
                kind: SlashCommandKind::Host(HostCommand::Redo),
            },
            SlashCommand {
                name: "goal".to_owned(),
                aliases: Vec::new(),
                description: SessionCommand::Goal.description().to_owned(),
                kind: SlashCommandKind::Host(HostCommand::Goal(String::new())),
            },
            SlashCommand {
                name: "preset".to_owned(),
                aliases: Vec::new(),
                description: "Switch the configured model team, or choose one".to_owned(),
                kind: SlashCommandKind::Host(HostCommand::Preset(None)),
            },
            SlashCommand {
                name: "council".to_owned(),
                aliases: Vec::new(),
                description: "Run a native multi-agent Council preset".to_owned(),
                kind: SlashCommandKind::Host(HostCommand::Council(String::new())),
            },
            SlashCommand {
                name: "plan".to_owned(),
                aliases: Vec::new(),
                description: "Enter Plan mode, or confirm starting work when already planning"
                    .to_owned(),
                kind: SlashCommandKind::Host(HostCommand::Plan),
            },
            SlashCommand {
                name: "start-plan".to_owned(),
                aliases: Vec::new(),
                description: "Enter read-only Plan mode immediately".to_owned(),
                kind: SlashCommandKind::Host(HostCommand::StartPlan),
            },
            SlashCommand {
                name: "start-work".to_owned(),
                aliases: Vec::new(),
                description: "Review the durable plan and confirm implementation".to_owned(),
                kind: SlashCommandKind::Host(HostCommand::StartWork),
            },
            SlashCommand {
                name: "stop".to_owned(),
                aliases: Vec::new(),
                description: "Stop one background terminal, or choose one".to_owned(),
                kind: SlashCommandKind::Host(HostCommand::Stop(None)),
            },
        ])
        .collect()
}

fn canonical_name(action: &str) -> String {
    // A command palette is a collection, while `/command` reads like a placeholder for
    // an invocation. Keep that one plural; resource pickers use the singular word below.
    if action == "command_list" {
        return "commands".to_owned();
    }
    if let Some(stem) = action.strip_suffix("_list") {
        return stem.replace('_', "-");
    }
    for suffix in ["_open", "_show", "_view"] {
        if let Some(stem) = action.strip_suffix(suffix) {
            return stem.replace('_', "-");
        }
    }
    action
        .rsplit('_')
        .next()
        .unwrap_or(action)
        .replace('_', "-")
}

/// Product capability owning a command name or keybind action.
///
/// This is a family classifier, not a list of forbidden spellings. Names are tokenised
/// on every separator the two registries use, so all members of a hosted, Console,
/// Workspace/Warp, move-session, or stash family are rejected at projection time. A new
/// member therefore inherits the boundary without changing this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandFamily {
    Local,
    Hosted,
    Workspace,
    Stash,
}

fn command_family(name: &str) -> CommandFamily {
    let tokens = name
        .split(['.', '-', '_'])
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    if tokens.iter().any(|token| {
        token.ends_with("share")
            || token.starts_with("console")
            || *token == "org"
            || *token == "connect"
            || token.starts_with("github")
    }) {
        return CommandFamily::Hosted;
    }
    if tokens.iter().any(|token| {
        token.starts_with("workspace") || token.starts_with("warp") || *token == "move"
    }) {
        return CommandFamily::Workspace;
    }
    if tokens.iter().any(|token| token.starts_with("stash")) {
        return CommandFamily::Stash;
    }
    CommandFamily::Local
}

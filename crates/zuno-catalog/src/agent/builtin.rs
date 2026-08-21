//! The seven native agents `opencode` defines before any user config is read.
//!
//! Oracle: `packages/opencode/src/agent/agent.ts:140-265`.
//!
//! Each built-in is reproduced with the fields that survive to `agent list` and to
//! the turn loop: its name, description, mode, hidden flag, temperature, and
//! prompt. The prompt files are byte-for-byte copies of the oracle's
//! `src/agent/prompt/*.txt`, verified by `md5sum` at import; three built-ins
//! (`build`, `plan`, `general`) genuinely have no prompt, and that absence is part
//! of their definition rather than an omission here.
//!
//! # What is deliberately not here
//!
//! Each built-in also carries a permission overlay — the `Permission.fromConfig`
//! literal at `agent.ts:145-152` for `build`, `:158-178` for `plan`, and so on —
//! which the oracle merges over a runtime-computed default set. That merge needs
//! `Truncate.GLOB`, the global tmp and plans directories, the discovered skill and
//! reference directories, and a worktree-relative rewrite. All of those belong to
//! the permission tasks, so [`Builtin::permission_overlay`] exposes the overlay as
//! declarative data and resolution is left to them. Modelling it as data here
//! keeps the oracle's literal in one place without this module growing a
//! permission engine.

use crate::agent::AgentMode;
use zuno_config::schema::ordered::OrderedMap;
use zuno_config::schema::permission::{
    PermissionAction, PermissionConfig, PermissionObject, PermissionRule,
};

/// `agent.ts:184` — `src/agent/prompt/explore.txt`.
pub const PROMPT_EXPLORE: &str = include_str!("prompt/explore.txt");
/// `agent.ts:192` — `src/agent/prompt/compaction.txt`.
pub const PROMPT_COMPACTION: &str = include_str!("prompt/compaction.txt");
/// `agent.ts:214` — `src/agent/prompt/title.txt`.
pub const PROMPT_TITLE: &str = include_str!("prompt/title.txt");
/// `agent.ts:229` — `src/agent/prompt/summary.txt`.
pub const PROMPT_SUMMARY: &str = include_str!("prompt/summary.txt");

/// The seven built-in names, in the order `agent.ts:142-232` declares them.
///
/// Declaration order is not display order: `agent list` sorts natives
/// alphabetically. It is kept because it is the order the oracle's object literal
/// establishes, and a later task that reproduces the object's own iteration will
/// need it.
pub const BUILTIN_NAMES: [&str; 7] = [
    "build",
    "plan",
    "general",
    "explore",
    "compaction",
    "title",
    "summary",
];

/// One native agent, as the oracle defines it before user config is applied.
#[derive(Debug, Clone, PartialEq)]
pub struct Builtin {
    /// The agent's name, which is also its config key.
    pub name: &'static str,
    /// When to use the agent. Absent for the three hidden utility agents.
    pub description: Option<&'static str>,
    /// Where the agent may be used.
    pub mode: AgentMode,
    /// Hidden from the `@` autocomplete menu.
    pub hidden: bool,
    /// Sampling temperature, set only for `title`.
    pub temperature: Option<f64>,
    /// The system prompt, absent for `build`, `plan`, and `general`.
    pub prompt: Option<&'static str>,
}

/// Every built-in, in declaration order.
#[must_use]
pub fn all() -> Vec<Builtin> {
    vec![
        build(),
        plan(),
        general(),
        explore(),
        compaction(),
        title(),
        summary(),
    ]
}

/// The built-in named `name`, if there is one.
#[must_use]
pub fn get(name: &str) -> Option<Builtin> {
    all().into_iter().find(|builtin| builtin.name == name)
}

/// Whether `name` is one of the seven natives.
#[must_use]
pub fn is_builtin(name: &str) -> bool {
    BUILTIN_NAMES.contains(&name)
}

/// `agent.ts:142-156`.
fn build() -> Builtin {
    Builtin {
        name: "build",
        description: Some("The default agent. Executes tools based on configured permissions."),
        mode: AgentMode::Primary,
        hidden: false,
        temperature: None,
        prompt: None,
    }
}

/// `agent.ts:157-181`.
fn plan() -> Builtin {
    Builtin {
        name: "plan",
        description: Some("Plan mode. Disallows all edit tools."),
        mode: AgentMode::Primary,
        hidden: false,
        temperature: None,
        prompt: None,
    }
}

/// `agent.ts:182-195`.
fn general() -> Builtin {
    Builtin {
        name: "general",
        description: Some(
            "General-purpose agent for researching complex questions and executing multi-step \
             tasks. Use this agent to execute multiple units of work in parallel.",
        ),
        mode: AgentMode::Subagent,
        hidden: false,
        temperature: None,
        prompt: None,
    }
}

/// `agent.ts:196-217`.
fn explore() -> Builtin {
    Builtin {
        name: "explore",
        description: Some(
            "Fast agent specialized for exploring codebases. Use this when you need to quickly \
             find files by patterns (eg. \"src/components/**/*.tsx\"), search code for keywords \
             (eg. \"API endpoints\"), or answer questions about the codebase (eg. \"how do API \
             endpoints work?\"). When calling this agent, specify the desired thoroughness level: \
             \"quick\" for basic searches, \"medium\" for moderate exploration, or \"very \
             thorough\" for comprehensive analysis across multiple locations and naming \
             conventions.",
        ),
        mode: AgentMode::Subagent,
        hidden: false,
        temperature: None,
        prompt: Some(PROMPT_EXPLORE),
    }
}

/// `agent.ts:218-232`.
fn compaction() -> Builtin {
    Builtin {
        name: "compaction",
        description: None,
        mode: AgentMode::Primary,
        hidden: true,
        temperature: None,
        prompt: Some(PROMPT_COMPACTION),
    }
}

/// `agent.ts:233-248`.
fn title() -> Builtin {
    Builtin {
        name: "title",
        description: None,
        mode: AgentMode::Primary,
        hidden: true,
        temperature: Some(0.5),
        prompt: Some(PROMPT_TITLE),
    }
}

/// `agent.ts:249-263`.
fn summary() -> Builtin {
    Builtin {
        name: "summary",
        description: None,
        mode: AgentMode::Primary,
        hidden: true,
        temperature: None,
        prompt: Some(PROMPT_SUMMARY),
    }
}

impl Builtin {
    /// The overlay the oracle merges over the runtime default permission set.
    ///
    /// Only the parts that do not depend on runtime paths are expressed. The
    /// path-dependent entries — `plan`'s `edit` and `external_directory` globs
    /// (`agent.ts:163-176`) and `explore`'s `external_directory` whitelist
    /// (`agent.ts:206`) — need the global data directory, the worktree, and the
    /// discovered skill and reference directories, and are therefore left to the
    /// permission tasks. Every built-in whose overlay is fully static returns it
    /// complete.
    #[must_use]
    pub fn permission_overlay(&self) -> Option<PermissionConfig> {
        let rules: Vec<(&str, PermissionRule)> = match self.name {
            // agent.ts:146-151
            "build" => vec![("question", allow()), ("plan_enter", allow())],
            // agent.ts:160-177 — the static prefix only; see the note above.
            "plan" => vec![
                ("question", allow()),
                ("plan_exit", allow()),
                ("task", patterns([("general", PermissionAction::Deny)])),
            ],
            // agent.ts:186-188
            "general" => vec![("todowrite", deny())],
            // agent.ts:199-209 — the static prefix only; see the note above.
            "explore" => vec![
                ("*", deny()),
                ("grep", allow()),
                ("glob", allow()),
                ("list", allow()),
                ("bash", allow()),
                ("webfetch", allow()),
                ("web_search", allow()),
                ("read", allow()),
            ],
            // agent.ts:221, :241, :256 — all three deny everything.
            "compaction" | "title" | "summary" => vec![("*", deny())],
            _ => return None,
        };
        let mut object = OrderedMap::new();
        for (key, rule) in rules {
            object.insert(key, rule);
        }
        Some(PermissionConfig::Object(PermissionObject(object)))
    }

    /// Whether [`Self::permission_overlay`] omits runtime-path-dependent entries.
    ///
    /// `true` for `plan` and `explore`. A caller that needs the complete ruleset
    /// must go through the permission tasks rather than treating the overlay as
    /// final.
    #[must_use]
    pub fn permission_overlay_is_partial(&self) -> bool {
        matches!(self.name, "plan" | "explore")
    }
}

fn allow() -> PermissionRule {
    PermissionRule::Action(PermissionAction::Allow)
}

fn deny() -> PermissionRule {
    PermissionRule::Action(PermissionAction::Deny)
}

fn patterns<const N: usize>(entries: [(&str, PermissionAction); N]) -> PermissionRule {
    let mut map = OrderedMap::new();
    for (pattern, action) in entries {
        map.insert(pattern, action);
    }
    PermissionRule::Patterns(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn there_are_exactly_seven_built_ins_and_the_names_match_the_index() {
        let all = all();
        assert_eq!(all.len(), 7);
        let names: Vec<&str> = all.iter().map(|builtin| builtin.name).collect();
        assert_eq!(names, BUILTIN_NAMES.to_vec());
    }

    #[test]
    fn every_built_in_is_reachable_by_name() {
        for name in BUILTIN_NAMES {
            assert!(is_builtin(name), "{name} should be a built-in");
            assert_eq!(get(name).expect("built-in exists").name, name);
        }
        assert!(!is_builtin("review/security"));
        assert!(get("review/security").is_none());
    }

    #[test]
    fn the_three_hidden_utility_agents_are_the_only_hidden_ones() {
        let hidden: Vec<&str> = all()
            .iter()
            .filter(|builtin| builtin.hidden)
            .map(|builtin| builtin.name)
            .collect();
        assert_eq!(hidden, vec!["compaction", "title", "summary"]);
    }

    #[test]
    fn only_title_sets_a_temperature() {
        for builtin in all() {
            let expected = if builtin.name == "title" {
                Some(0.5)
            } else {
                None
            };
            assert_eq!(builtin.temperature, expected, "for {}", builtin.name);
        }
    }

    #[test]
    fn the_four_prompt_bearing_built_ins_carry_non_empty_prompts() {
        let with_prompt: Vec<&str> = all()
            .iter()
            .filter(|builtin| builtin.prompt.is_some())
            .map(|builtin| builtin.name)
            .collect();
        assert_eq!(
            with_prompt,
            vec!["explore", "compaction", "title", "summary"]
        );
        for builtin in all() {
            if let Some(prompt) = builtin.prompt {
                assert!(
                    prompt.len() > 200,
                    "{} prompt is {} bytes, which is too short to be the oracle's",
                    builtin.name,
                    prompt.len()
                );
            }
        }
    }

    #[test]
    fn the_prompt_files_are_the_oracle_texts_not_paraphrases() {
        // Byte lengths taken from `wc -c` on the oracle's prompt directory. A
        // paraphrased or truncated prompt changes agent behaviour silently, so the
        // sizes are pinned alongside anchor phrases.
        assert_eq!(PROMPT_COMPACTION.len(), 823);
        assert_eq!(PROMPT_EXPLORE.len(), 871);
        assert_eq!(PROMPT_SUMMARY.len(), 648);
        assert_eq!(PROMPT_TITLE.len(), 2120);
        assert!(PROMPT_COMPACTION.starts_with("You are an anchored context summarization"));
        assert!(PROMPT_EXPLORE.starts_with("You are a file search specialist."));
        assert!(PROMPT_SUMMARY.starts_with("Summarize what was done in this conversation."));
        assert!(PROMPT_TITLE.contains("≤50 characters"));
        assert!(PROMPT_TITLE.trim_end().ends_with("</examples>"));
    }

    #[test]
    fn the_three_prompt_less_built_ins_still_describe_themselves() {
        for name in ["build", "plan", "general"] {
            let builtin = get(name).expect("built-in exists");
            assert!(builtin.prompt.is_none(), "{name} should have no prompt");
            assert!(
                builtin.description.is_some_and(|text| !text.is_empty()),
                "{name} must have a description"
            );
        }
    }

    #[test]
    fn modes_match_the_oracle_literal() {
        let modes: Vec<(&str, AgentMode)> = all()
            .iter()
            .map(|builtin| (builtin.name, builtin.mode))
            .collect();
        assert_eq!(
            modes,
            vec![
                ("build", AgentMode::Primary),
                ("plan", AgentMode::Primary),
                ("general", AgentMode::Subagent),
                ("explore", AgentMode::Subagent),
                ("compaction", AgentMode::Primary),
                ("title", AgentMode::Primary),
                ("summary", AgentMode::Primary),
            ]
        );
    }

    #[test]
    fn every_built_in_has_a_permission_overlay_and_two_are_marked_partial() {
        let partial: Vec<&str> = all()
            .iter()
            .filter(|builtin| {
                assert!(
                    builtin.permission_overlay().is_some(),
                    "{} needs an overlay",
                    builtin.name
                );
                builtin.permission_overlay_is_partial()
            })
            .map(|builtin| builtin.name)
            .collect();
        assert_eq!(partial, vec!["plan", "explore"]);
    }

    #[test]
    fn explore_denies_everything_before_allowing_its_seven_tools() {
        let overlay = get("explore")
            .expect("explore exists")
            .permission_overlay()
            .expect("explore has an overlay");
        let object = overlay.normalized();
        let keys: Vec<&str> = object.iter().map(|(key, _)| key).collect();
        assert_eq!(
            keys,
            vec![
                "*",
                "grep",
                "glob",
                "list",
                "bash",
                "webfetch",
                "web_search",
                "read"
            ],
            "the wildcard deny must come first or the allows are overridden"
        );
    }
}

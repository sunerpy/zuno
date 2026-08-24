//! Zuno's native agent catalog.
//!
//! The catalog owns agent identity, selection mode, base prompt, and the permission
//! overlay applied by the composition root. Delegation-specific policy lives in
//! `zuno-agent`; its tests assert that every delegable name resolves here. Keeping
//! the catalog as the identity source prevents a tool from advertising an agent that
//! a child turn cannot start.

use crate::agent::AgentMode;
use zuno_config::schema::ordered::OrderedMap;
use zuno_config::schema::permission::{
    PermissionAction, PermissionConfig, PermissionMode, PermissionObject, PermissionRule,
};

/// End-to-end implementation agent.
pub const PROMPT_BUILD: &str = include_str!("prompt/build.txt");
/// Read-only planning agent.
pub const PROMPT_PLAN: &str = include_str!("prompt/plan.txt");
/// Thorough cross-cutting implementation agent.
pub const PROMPT_DEEP: &str = include_str!("prompt/deep.txt");
/// Repository exploration specialist.
pub const PROMPT_EXPLORER: &str = include_str!("prompt/explorer.txt");
/// External research specialist.
pub const PROMPT_LIBRARIAN: &str = include_str!("prompt/librarian.txt");
/// Architecture and review specialist.
pub const PROMPT_ADVISOR: &str = include_str!("prompt/advisor.txt");
/// Bounded implementation specialist.
pub const PROMPT_WORKER: &str = include_str!("prompt/worker.txt");
/// Visual artifact specialist.
pub const PROMPT_LOOKER: &str = include_str!("prompt/looker.txt");
/// Context compaction agent.
pub const PROMPT_COMPACTION: &str = include_str!("prompt/compaction.txt");
/// Session title agent.
pub const PROMPT_TITLE: &str = include_str!("prompt/title.txt");
/// Session summary agent.
pub const PROMPT_SUMMARY: &str = include_str!("prompt/summary.txt");

/// Native names in deterministic declaration order.
pub const BUILTIN_NAMES: [&str; 11] = [
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
    "summary",
];

/// One native agent before user configuration is applied.
#[derive(Debug, Clone, PartialEq)]
pub struct Builtin {
    /// Selection name and configuration key.
    pub name: &'static str,
    /// When the agent should be selected.
    pub description: Option<&'static str>,
    /// Whether the agent is primary, delegable, or both.
    pub mode: AgentMode,
    /// Whether user-facing selectors omit the agent.
    pub hidden: bool,
    /// Sampling temperature when Zuno chooses one explicitly.
    pub temperature: Option<f64>,
    /// Base system prompt.
    pub prompt: Option<&'static str>,
}

/// Every native agent in declaration order.
#[must_use]
pub fn all() -> Vec<Builtin> {
    vec![
        build(),
        plan(),
        deep(),
        explorer(),
        librarian(),
        advisor(),
        worker(),
        looker(),
        compaction(),
        title(),
        summary(),
    ]
}

/// The native agent named `name`.
#[must_use]
pub fn get(name: &str) -> Option<Builtin> {
    all().into_iter().find(|builtin| builtin.name == name)
}

/// Whether `name` is native.
#[must_use]
pub fn is_builtin(name: &str) -> bool {
    BUILTIN_NAMES.contains(&name)
}

fn build() -> Builtin {
    Builtin {
        name: "build",
        description: Some(
            "Owns a development request end to end: investigates, delegates bounded work, \
             edits, verifies, and reports only after the requested outcome is real.",
        ),
        mode: AgentMode::Primary,
        hidden: false,
        temperature: Some(0.1),
        prompt: Some(PROMPT_BUILD),
    }
}

fn plan() -> Builtin {
    Builtin {
        name: "plan",
        description: Some(
            "Researches the repository and produces an implementation-ready plan without \
             changing product files.",
        ),
        mode: AgentMode::Primary,
        hidden: false,
        temperature: Some(0.1),
        prompt: Some(PROMPT_PLAN),
    }
}

fn deep() -> Builtin {
    Builtin {
        name: "deep",
        description: Some(
            "Handles ambiguous or cross-cutting work that needs sustained investigation, \
             implementation, and verification in one bounded child session.",
        ),
        mode: AgentMode::Subagent,
        hidden: false,
        temperature: Some(0.1),
        prompt: Some(PROMPT_DEEP),
    }
}

fn explorer() -> Builtin {
    Builtin {
        name: "explorer",
        description: Some(
            "Maps repository structure, definitions, callers, and change impact without \
             modifying the working tree.",
        ),
        mode: AgentMode::Subagent,
        hidden: false,
        temperature: Some(0.1),
        prompt: Some(PROMPT_EXPLORER),
    }
}

fn librarian() -> Builtin {
    Builtin {
        name: "librarian",
        description: Some(
            "Researches current external documentation, releases, standards, and upstream \
             implementations with explicit source and version evidence.",
        ),
        mode: AgentMode::Subagent,
        hidden: false,
        temperature: Some(0.1),
        prompt: Some(PROMPT_LIBRARIAN),
    }
}

fn advisor() -> Builtin {
    Builtin {
        name: "advisor",
        description: Some(
            "Reviews code and architecture, surfaces concrete failure modes, compares \
             alternatives, and recommends one trade-off explicitly.",
        ),
        mode: AgentMode::Subagent,
        hidden: false,
        temperature: Some(0.4),
        prompt: Some(PROMPT_ADVISOR),
    }
}

fn worker() -> Builtin {
    Builtin {
        name: "worker",
        description: Some(
            "Completes a bounded, well-specified code change by reading, editing, testing, and \
             reporting exact verification results.",
        ),
        mode: AgentMode::Subagent,
        hidden: false,
        temperature: Some(0.1),
        prompt: Some(PROMPT_WORKER),
    }
}

fn looker() -> Builtin {
    Builtin {
        name: "looker",
        description: Some(
            "Inspects images, screenshots, PDFs, and diagrams and returns only the visual \
             evidence relevant to the caller's question.",
        ),
        mode: AgentMode::Subagent,
        hidden: false,
        temperature: Some(0.2),
        prompt: Some(PROMPT_LOOKER),
    }
}

fn compaction() -> Builtin {
    Builtin {
        name: "compaction",
        description: None,
        mode: AgentMode::Primary,
        hidden: true,
        temperature: Some(0.1),
        prompt: Some(PROMPT_COMPACTION),
    }
}

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

fn summary() -> Builtin {
    Builtin {
        name: "summary",
        description: None,
        mode: AgentMode::Primary,
        hidden: true,
        temperature: Some(0.1),
        prompt: Some(PROMPT_SUMMARY),
    }
}

impl Builtin {
    /// Native permission overlay merged after the common defaults.
    ///
    /// Every subagent is deny-by-default. The primary `build` agent inherits the
    /// common tool set and may delegate. `plan` may inspect and write only its plan
    /// document; the path-specific edit grants are added by the CLI composition
    /// root.
    #[must_use]
    pub fn permission_overlay(&self) -> Option<PermissionConfig> {
        let rules: Vec<(&str, PermissionRule)> = match self.name {
            "build" => vec![
                ("question", allow()),
                ("plan_enter", allow()),
                ("task", allow()),
                ("plan_get", allow()),
                ("plan_update", allow()),
                ("todo_get", allow()),
                ("todo_update", allow()),
            ],
            "plan" => vec![
                ("*", deny()),
                ("read", allow()),
                ("glob", allow()),
                ("grep", allow()),
                ("lsp", allow()),
                ("webfetch", allow()),
                ("web_search", allow()),
                ("question", allow()),
                ("plan_exit", allow()),
                ("goal_get", allow()),
                ("plan_get", allow()),
                ("plan_update", allow()),
                ("todo_get", allow()),
                ("todo_update", allow()),
                ("skill", allow()),
            ],
            "deep" | "worker" => vec![
                ("*", deny()),
                ("read", allow()),
                ("glob", allow()),
                ("grep", allow()),
                ("lsp", allow()),
                ("edit", allow()),
                ("bash", allow()),
                ("webfetch", allow()),
                ("web_search", allow()),
                ("plan_get", allow()),
                ("plan_update", allow()),
                ("todo_get", allow()),
                ("todo_update", allow()),
                ("skill", allow()),
                ("execute", allow()),
            ],
            "explorer" => vec![
                ("*", deny()),
                ("read", allow()),
                ("glob", allow()),
                ("grep", allow()),
                ("lsp", allow()),
            ],
            "librarian" => vec![
                ("*", deny()),
                ("read", allow()),
                ("glob", allow()),
                ("grep", allow()),
                ("lsp", allow()),
                ("webfetch", allow()),
                ("web_search", allow()),
            ],
            "advisor" | "looker" => vec![
                ("*", deny()),
                ("read", allow()),
                ("glob", allow()),
                ("grep", allow()),
                ("lsp", allow()),
            ],
            "compaction" | "title" | "summary" => vec![("*", deny())],
            _ => return None,
        };
        let mut object = OrderedMap::new();
        for (key, rule) in rules {
            object.insert(key, rule);
        }
        Some(PermissionConfig {
            mode: PermissionMode::Standard,
            rules: PermissionObject(object),
        })
    }

    /// Whether the composition root must add runtime path rules.
    #[must_use]
    pub fn permission_overlay_is_partial(&self) -> bool {
        matches!(
            self.name,
            "plan" | "explorer" | "librarian" | "advisor" | "looker"
        )
    }
}

fn allow() -> PermissionRule {
    PermissionRule::Action(PermissionAction::Allow)
}

fn deny() -> PermissionRule {
    PermissionRule::Action(PermissionAction::Deny)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_names_and_definitions_are_one_table() {
        let all = all();
        assert_eq!(all.len(), BUILTIN_NAMES.len());
        assert_eq!(
            all.iter().map(|builtin| builtin.name).collect::<Vec<_>>(),
            BUILTIN_NAMES
        );
        for name in BUILTIN_NAMES {
            assert_eq!(get(name).expect("native exists").name, name);
        }
    }

    #[test]
    fn only_engine_internals_are_hidden() {
        let hidden = all()
            .into_iter()
            .filter(|builtin| builtin.hidden)
            .map(|builtin| builtin.name)
            .collect::<Vec<_>>();
        assert_eq!(hidden, vec!["compaction", "title", "summary"]);
    }

    #[test]
    fn every_native_has_a_non_empty_prompt_and_permission_overlay() {
        for builtin in all() {
            assert!(
                builtin
                    .prompt
                    .is_some_and(|prompt| !prompt.trim().is_empty()),
                "{} must have a prompt",
                builtin.name
            );
            assert!(
                builtin.permission_overlay().is_some(),
                "{} must have an executable permission policy",
                builtin.name
            );
        }
    }

    #[test]
    fn subagents_are_deny_by_default_and_cannot_delegate() {
        for builtin in all()
            .into_iter()
            .filter(|builtin| builtin.mode == AgentMode::Subagent)
        {
            let overlay = builtin.permission_overlay().expect("overlay").rules;
            let first = overlay.iter().next().expect("at least wildcard deny");
            assert_eq!(first.0, "*", "{} needs a wildcard deny", builtin.name);
            assert_eq!(
                first.1,
                &PermissionRule::Action(PermissionAction::Deny),
                "{} needs deny-by-default",
                builtin.name
            );
            assert!(
                !overlay.iter().any(|(key, rule)| {
                    key == "task" && rule == &PermissionRule::Action(PermissionAction::Allow)
                }),
                "{} must not create grandchildren",
                builtin.name
            );
        }
    }

    #[test]
    fn plan_is_read_only_by_capability_not_only_by_prompt() {
        let overlay = get("plan")
            .expect("plan")
            .permission_overlay()
            .expect("overlay")
            .rules;
        assert_eq!(
            overlay.iter().next(),
            Some(("*", &PermissionRule::Action(PermissionAction::Deny)))
        );
        for allowed in [
            "read",
            "glob",
            "grep",
            "lsp",
            "webfetch",
            "web_search",
            "question",
            "goal_get",
            "plan_get",
            "plan_update",
            "todo_get",
            "todo_update",
            "skill",
        ] {
            assert_eq!(
                overlay.get(allowed),
                Some(&PermissionRule::Action(PermissionAction::Allow)),
                "Plan mode must expose `{allowed}`"
            );
        }
        for denied in ["bash", "write", "edit", "patch", "task", "execute"] {
            assert_ne!(
                overlay.get(denied),
                Some(&PermissionRule::Action(PermissionAction::Allow)),
                "Plan mode unexpectedly allows `{denied}`"
            );
        }
    }

    #[test]
    fn build_plan_and_deep_are_first_class_agents() {
        assert_eq!(get("build").expect("build").mode, AgentMode::Primary);
        assert_eq!(get("plan").expect("plan").mode, AgentMode::Primary);
        assert_eq!(get("deep").expect("deep").mode, AgentMode::Subagent);
    }

    #[test]
    fn delivery_prompts_require_evidence_without_becoming_policy_dumps() {
        let cases = [
            (
                "build",
                PROMPT_BUILD,
                270,
                [
                    "Do not declare completion from intent",
                    "authoritative evidence",
                    "Do not duplicate delegated discovery",
                    "use apply_patch",
                ],
            ),
            (
                "plan",
                PROMPT_PLAN,
                190,
                [
                    "without modifying product files",
                    "authoritative evidence",
                    "implementation decision",
                    "Remove obsolete paths",
                ],
            ),
            (
                "deep",
                PROMPT_DEEP,
                210,
                [
                    "without delegating",
                    "owning abstraction",
                    "interruption and recovery",
                    "use write only for a new file",
                ],
            ),
        ];

        for (name, prompt, word_limit, clauses) in cases {
            for clause in clauses {
                assert!(
                    prompt.contains(clause),
                    "{name} prompt is missing `{clause}`:\n{prompt}"
                );
            }
            let words = prompt.split_whitespace().count();
            assert!(
                words <= word_limit,
                "{name} prompt grew to {words} words; concise role policy belongs here, not a \
                 second harness manual"
            );
        }
    }

    #[test]
    fn specialist_prompts_define_evidence_output_and_scope_boundaries() {
        let cases = [
            (
                "explorer",
                PROMPT_EXPLORER,
                130,
                [
                    "actual runtime path",
                    "what the code proves",
                    "Do not browse",
                ],
            ),
            (
                "librarian",
                PROMPT_LIBRARIAN,
                130,
                ["exact version", "final authority", "may drift over time"],
            ),
            (
                "advisor",
                PROMPT_ADVISOR,
                150,
                ["ownership boundaries", "demonstrated defect", "Do not edit"],
            ),
            (
                "worker",
                PROMPT_WORKER,
                160,
                ["scope boundary", "use write only", "uncertain side effect"],
            ),
            (
                "looker",
                PROMPT_LOOKER,
                130,
                ["full artifact", "direct observation", "Do not edit"],
            ),
        ];

        for (name, prompt, word_limit, clauses) in cases {
            for clause in clauses {
                assert!(
                    prompt.contains(clause),
                    "{name} prompt is missing `{clause}`:\n{prompt}"
                );
            }
            let words = prompt.split_whitespace().count();
            assert!(
                words <= word_limit,
                "{name} prompt grew to {words} words; keep role guidance compact"
            );
        }
    }
}

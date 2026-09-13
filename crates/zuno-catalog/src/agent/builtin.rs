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

// Working roles share one verification rubric. Hidden tool-free roles retain
// their output-specific prompts, and configured prompt overrides remain intact.
// Design references at Codex eaa8b6d917: codex-rs/models-manager/prompt.md
// ("Validating your work"), and codex-rs/prompts/src/review_request.rs::REVIEW_PROMPT
// with codex-rs/prompts/templates/review/rubric.md. The stricter reproducible-bug
// red/green sequence is user-chosen Zuno prompt guidance, not a Codex runtime gate.
macro_rules! working_prompt {
    ($body:expr) => {
        concat!($body, "\n\n", include_str!("prompt/verification.txt"))
    };
}

macro_rules! specialist_prompt {
    ($path:literal) => {
        working_prompt!(concat!(
            include_str!($path),
            "\n\nReturn concise natural Markdown. Use these headings when they add value: \
             Outcome, Evidence, Inspected/Changed, Risks/Blocker. Omit empty headings. Do not \
             emit JSON or XML unless the caller explicitly requires machine-readable output."
        ))
    };
}

/// Default multi-agent coordinator.
pub const PROMPT_ORCHESTRATOR: &str = working_prompt!(concat!(
    include_str!("prompt/orchestrator.txt"),
    "\n\n",
    include_str!("prompt/task-contract.txt")
));
/// Direct end-to-end implementation agent.
pub const PROMPT_BUILD: &str = working_prompt!(include_str!("prompt/build.txt"));
/// Read-only planning agent.
pub const PROMPT_PLAN: &str = working_prompt!(include_str!("prompt/plan.txt"));
/// Read-only high-assurance review agent.
pub const PROMPT_REVIEW: &str = working_prompt!(include_str!("prompt/review.txt"));
/// Thorough cross-cutting implementation agent.
pub const PROMPT_DEEP: &str = working_prompt!(concat!(
    include_str!("prompt/deep.txt"),
    "\n\n",
    include_str!("prompt/task-contract.txt")
));
/// Focused local implementation specialist.
pub const PROMPT_FIXER: &str = specialist_prompt!("prompt/fixer.txt");
/// Bounded miscellaneous implementation specialist.
pub const PROMPT_GENERAL: &str = specialist_prompt!("prompt/general.txt");
/// Repository exploration specialist.
pub const PROMPT_EXPLORER: &str = specialist_prompt!("prompt/explorer.txt");
/// External research specialist.
pub const PROMPT_LIBRARIAN: &str = specialist_prompt!("prompt/librarian.txt");
/// Architecture and review specialist.
pub const PROMPT_ORACLE: &str = specialist_prompt!("prompt/oracle.txt");
/// Visual artifact specialist.
pub const PROMPT_LOOKER: &str = specialist_prompt!("prompt/looker.txt");
/// Context compaction agent.
pub const PROMPT_COMPACTION: &str = include_str!("prompt/compaction.txt");
/// Session title agent.
pub const PROMPT_TITLE: &str = include_str!("prompt/title.txt");
/// Session summary agent.
pub const PROMPT_SUMMARY: &str = include_str!("prompt/summary.txt");
/// Tool-free Council synthesis agent.
pub const PROMPT_COUNCIL_SYNTH: &str = include_str!("prompt/council-synth.txt");

/// Native names in deterministic declaration order.
pub const BUILTIN_NAMES: [&str; 15] = [
    "orchestrator",
    "build",
    "plan",
    "review",
    "deep",
    "fixer",
    "general",
    "explorer",
    "librarian",
    "oracle",
    "looker",
    "compaction",
    "title",
    "summary",
    "council-synth",
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
    /// Exact child-Agent allowlist. `None` means delegation is not declared.
    pub delegates: Option<&'static [&'static str]>,
}

/// Bounded work targets for delivery and deep investigation. The runtime applies
/// the caller's tool authority, configured depth, and per-Agent restrictions.
const WORK_DELEGATES: &[&str] = &[
    "deep",
    "fixer",
    "general",
    "explorer",
    "librarian",
    "oracle",
    "looker",
];

/// The seats `review` runs, and nothing else.
///
/// Exactly the three agents the first-party `balanced-review` Council preset seats, in
/// its order: implementation evidence, contract evidence, decision review. Council
/// validation refuses a preset whose seat agent is outside the caller's allowlist, so
/// this list is what lets `review` run that preset at all. It deliberately excludes
/// `review` itself — a review that could seat another review would recurse — and every
/// mutating specialist, because a read-only agent must not reach a writing child.
const REVIEW_DELEGATES: &[&str] = &["explorer", "librarian", "oracle"];

/// The natives that may start a child turn at all.
///
/// `orchestrator` coordinates delivery; `deep` may delegate bounded evidence or
/// implementation work while retaining the causal investigation and verification.
/// `review` seats only the first-party `balanced-review` Council. `build` remains
/// a direct execution lane without child tools.
///
/// One list so the catalog and its tests cannot disagree about where the boundary is.
pub const DELEGATING_NATIVES: [&str; 3] = ["orchestrator", "review", "deep"];

/// The built-in Skills `review` loads before it starts reasoning.
///
/// Both are first-party. That is the point: `required_skills` resolution fails closed on
/// a name that matches nothing, so requiring the user-installed `codegraph` Skill would
/// make the review agent unavailable on every machine that does not happen to have it.
/// `codemap` reaches the same CodeGraph index through a first-party read-only interface,
/// and a `codegraph` Skill that *is* installed still gets discovered the ordinary way.
///
/// A declared Skill only reaches the model when this agent's overlay also grants that
/// Skill's required tools, which is why the `review` overlay allows `read`, `glob`,
/// `grep` and `skill`.
const REVIEW_REQUIRED_SKILLS: &[&str] = &["codemap", "verification-planning"];

/// First-party decomposition and acceptance disciplines for deep work.
const DEEP_REQUIRED_SKILLS: &[&str] = &["deepwork", "verification-planning"];

/// Every native agent in declaration order.
#[must_use]
pub fn all() -> Vec<Builtin> {
    vec![
        orchestrator(),
        build(),
        plan(),
        review(),
        deep(),
        fixer(),
        general(),
        explorer(),
        librarian(),
        oracle(),
        looker(),
        compaction(),
        title(),
        summary(),
        council_synth(),
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

fn orchestrator() -> Builtin {
    Builtin {
        name: "orchestrator",
        description: Some(
            "Coordinates non-trivial delivery: builds a dependency graph, delegates bounded \
             non-overlapping work, integrates results, and independently verifies completion.",
        ),
        mode: AgentMode::Primary,
        hidden: false,
        temperature: Some(0.1),
        prompt: Some(PROMPT_ORCHESTRATOR),
        delegates: Some(WORK_DELEGATES),
    }
}

fn build() -> Builtin {
    Builtin {
        name: "build",
        description: Some(
            "Owns one direct development lane end to end: investigates, edits, verifies, and \
             reports only after the requested outcome is real, without child Agents.",
        ),
        mode: AgentMode::Primary,
        hidden: false,
        temperature: Some(0.1),
        prompt: Some(PROMPT_BUILD),
        delegates: None,
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
        delegates: None,
    }
}

fn review() -> Builtin {
    Builtin {
        name: "review",
        description: Some(
            "Reviews a plan, design, or root-cause analysis against recorded evidence and \
             decides whether it is ready to implement, without changing product files.",
        ),
        mode: AgentMode::Primary,
        hidden: false,
        temperature: Some(0.1),
        prompt: Some(PROMPT_REVIEW),
        delegates: Some(REVIEW_DELEGATES),
    }
}

fn deep() -> Builtin {
    Builtin {
        name: "deep",
        description: Some(
            "Owns difficult debugging and cross-cutting work through evidence, competing \
             hypotheses, discriminating experiments, authorized root fixes, and recovery verification; \
             may delegate bounded tasks when runtime authority permits.",
        ),
        mode: AgentMode::All,
        hidden: false,
        temperature: Some(0.1),
        prompt: Some(PROMPT_DEEP),
        delegates: Some(WORK_DELEGATES),
    }
}

fn fixer() -> Builtin {
    Builtin {
        name: "fixer",
        description: Some(
            "Completes a known local code change with the smallest sufficient patch and \
             focused regression evidence, without external research or delegation.",
        ),
        mode: AgentMode::Subagent,
        hidden: false,
        temperature: Some(0.1),
        prompt: Some(PROMPT_FIXER),
        delegates: None,
    }
}

fn general() -> Builtin {
    Builtin {
        name: "general",
        description: Some(
            "Completes one bounded miscellaneous deliverable that no narrower specialist \
             owns, under an explicit capability envelope and without child Agents.",
        ),
        mode: AgentMode::Subagent,
        hidden: false,
        temperature: Some(0.1),
        prompt: Some(PROMPT_GENERAL),
        delegates: None,
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
        delegates: None,
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
        delegates: None,
    }
}

fn oracle() -> Builtin {
    Builtin {
        name: "oracle",
        description: Some(
            "Reviews code and architecture, surfaces concrete failure modes, compares \
             alternatives, and recommends one trade-off explicitly.",
        ),
        mode: AgentMode::Subagent,
        hidden: false,
        temperature: Some(0.4),
        prompt: Some(PROMPT_ORACLE),
        delegates: None,
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
        delegates: None,
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
        delegates: None,
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
        delegates: None,
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
        delegates: None,
    }
}

fn council_synth() -> Builtin {
    Builtin {
        name: "council-synth",
        description: None,
        mode: AgentMode::Primary,
        hidden: true,
        temperature: Some(0.1),
        prompt: Some(PROMPT_COUNCIL_SYNTH),
        delegates: None,
    }
}

impl Builtin {
    /// Native permission overlay merged after the common defaults.
    ///
    /// Every delegable Agent is deny-by-default. The primary `orchestrator`
    /// inherits the common tool set and may delegate; direct `build` explicitly
    /// denies delegation. `deep` is directly selectable and delegable, may delegate
    /// bounded work within runtime limits, and may own the same durable Goal lifecycle
    /// as a primary writer. `plan` may inspect and update durable planning state without
    /// granting workspace edit tools.
    ///
    /// Two grants are here because a deny-by-default overlay hides anything it does not
    /// name, and both were unnamed. `bg` accompanies every `shell` grant: a background
    /// execution is started through `shell` and read back only through `bg`, so an
    /// overlay that granted one and not the other let an Agent start work it could not
    /// inspect, and left a withheld oversized result with no retrieval path. `job` is
    /// the mirror image — it resolves only a Job row whose parent session is the caller,
    /// and only a `task` call creates one — so `build` denies it alongside `task` rather
    /// than inheriting it from the common defaults.
    #[must_use]
    pub fn permission_overlay(&self) -> Option<PermissionConfig> {
        let mut rules: Vec<(&str, PermissionRule)> = match self.name {
            "orchestrator" => vec![
                ("plan_enter", allow()),
                ("task", allow()),
                ("plan_get", allow()),
                ("plan_update", allow()),
                ("todo_get", allow()),
                ("todo_update", allow()),
            ],
            "build" => vec![
                ("task", deny()),
                ("job", deny()),
                ("plan_enter", allow()),
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
                ("shell", allow()),
                ("bg", allow()),
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
            // Read-only like `plan`, plus the native Council and evidence surface a review
            // needs. Arbitrary `task` delegation remains hidden: a review reaches child
            // seats only through the configuration-owned balanced Council. `job`
            // accompanies it so the agent can inspect the resulting durable row.
            "review" => vec![
                ("*", deny()),
                ("read", allow()),
                ("glob", allow()),
                ("grep", allow()),
                ("lsp", allow()),
                ("shell", allow()),
                ("bg", allow()),
                ("webfetch", allow()),
                ("web_search", allow()),
                ("question", allow()),
                ("council_run", allow()),
                ("job", allow()),
                ("review_open", allow()),
                ("review_claim", allow()),
                ("review_get", allow()),
                ("review_finalize", allow()),
                ("task_report", allow()),
                ("goal_get", allow()),
                ("plan_get", allow()),
                ("todo_get", allow()),
                ("skill", allow()),
            ],
            "deep" => vec![
                ("*", deny()),
                ("read", allow()),
                ("glob", allow()),
                ("grep", allow()),
                ("lsp", allow()),
                ("edit", allow()),
                ("shell", allow()),
                ("bg", allow()),
                ("task", allow()),
                ("job", allow()),
                ("webfetch", allow()),
                ("web_search", allow()),
                ("question", allow()),
                ("goal_get", allow()),
                ("goal_propose", allow()),
                ("goal_update", allow()),
                ("goal_request_input", allow()),
                ("plan_get", allow()),
                ("plan_update", allow()),
                ("todo_get", allow()),
                ("todo_update", allow()),
                ("skill", allow()),
                ("execute", allow()),
            ],
            "general" => vec![
                ("*", deny()),
                ("read", allow()),
                ("glob", allow()),
                ("grep", allow()),
                ("lsp", allow()),
                ("edit", allow()),
                ("shell", allow()),
                ("bg", allow()),
                ("webfetch", allow()),
                ("web_search", allow()),
                ("plan_get", allow()),
                ("plan_update", allow()),
                ("todo_get", allow()),
                ("todo_update", allow()),
                ("skill", allow()),
                ("execute", allow()),
            ],
            "fixer" => vec![
                ("*", deny()),
                ("read", allow()),
                ("glob", allow()),
                ("grep", allow()),
                ("lsp", allow()),
                ("edit", allow()),
                ("shell", allow()),
                ("bg", allow()),
                ("plan_get", allow()),
                ("todo_get", allow()),
                ("skill", allow()),
            ],
            "explorer" => vec![
                ("*", deny()),
                ("read", allow()),
                ("glob", allow()),
                ("grep", allow()),
                ("lsp", allow()),
                ("shell", allow()),
                ("bg", allow()),
                ("skill", allow()),
            ],
            "librarian" => vec![
                ("*", deny()),
                ("read", allow()),
                ("glob", allow()),
                ("grep", allow()),
                ("lsp", allow()),
                ("shell", allow()),
                ("bg", allow()),
                ("webfetch", allow()),
                ("web_search", allow()),
                ("skill", allow()),
            ],
            "oracle" | "looker" => vec![
                ("*", deny()),
                ("read", allow()),
                ("glob", allow()),
                ("grep", allow()),
                ("lsp", allow()),
                ("shell", allow()),
                ("bg", allow()),
                ("skill", allow()),
            ],
            "compaction" | "title" | "summary" | "council-synth" => vec![("*", deny())],
            _ => return None,
        };
        // Reporting is a separate host-managed capability. It does not grant edit
        // tools or change the role's Shell filesystem contract.
        if !self.hidden {
            // Leaving the workspace is an auxiliary permission, not another tool.
            // A deny-by-default role must name it even when it already allows read
            // or shell. Ask follows the configured approval mode; it is never a
            // blanket grant over a later user or per-Agent deny.
            rules.push((
                "external_directory",
                PermissionRule::Action(PermissionAction::Ask),
            ));
            rules.push(("tool_search", allow()));
            rules.push(("report_write", allow()));
            rules.push(("memory_read", allow()));
            rules.push(("experience_search", allow()));
            if matches!(
                self.name,
                "orchestrator" | "build" | "deep" | "general" | "fixer"
            ) {
                rules.push(("memory_update", allow()));
            }
        }
        let mut object = OrderedMap::new();
        for (key, rule) in rules {
            object.insert(key, rule);
        }
        Some(PermissionConfig {
            mode: PermissionMode::Standard,
            rules: PermissionObject(object),
        })
    }

    /// Whether this working Agent needs the shared runtime directory rules.
    #[must_use]
    pub fn uses_external_directories(&self) -> bool {
        !self.hidden
    }

    /// Skills this native loads at the start of every turn.
    ///
    /// `None` leaves Skill selection to the model, which is right for an agent whose work
    /// is not defined by one discipline. `review` requires checkable evidence; `deep`
    /// requires durable decomposition and a verification plan for its causal work.
    ///
    /// Only first-party names may appear here; see [`REVIEW_REQUIRED_SKILLS`].
    #[must_use]
    pub fn required_skills(&self) -> Option<&'static [&'static str]> {
        match self.name {
            "review" => Some(REVIEW_REQUIRED_SKILLS),
            "deep" => Some(DEEP_REQUIRED_SKILLS),
            _ => None,
        }
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
    use zuno_permission::visibility::is_tool_visible;
    use zuno_permission::{Rule, rules_from_config};

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
        assert_eq!(
            hidden,
            vec!["compaction", "title", "summary", "council-synth"]
        );
        for name in hidden {
            let builtin = get(name).expect("internal exists");
            let rules = effective_rules(&builtin);
            assert!(builtin.delegates.is_none());
            assert!(builtin.required_skills().is_none());
            for tool in [
                "read",
                "shell",
                "edit",
                "task",
                "skill",
                "tool_search",
                "report_write",
                "memory_update",
                "unknown_tool",
            ] {
                assert!(
                    !is_tool_visible(tool, &rules),
                    "{name} must not expose {tool}"
                );
            }
        }
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
    fn delegable_agents_are_deny_by_default_and_only_deep_may_delegate() {
        for builtin in all()
            .into_iter()
            .filter(|builtin| matches!(builtin.mode, AgentMode::Subagent | AgentMode::All))
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
            assert_eq!(
                is_tool_visible("task", &effective_rules(&builtin)),
                builtin.name == "deep",
                "{} disagrees with its native delegation policy",
                builtin.name
            );
        }
    }

    /// The overlay decides `bg`, so the overlay is where the omission has to fail.
    ///
    /// Composed the way the composition root composes it: the common `"*": allow`
    /// default first, then this overlay. That is the whole chain that decides `bg` and
    /// `job`; the rest of the default set (`question`, `plan_enter`, `plan_exit`, the
    /// `read` patterns) is in `zuno-cli`'s `default_rules` and does not name either id.
    #[test]
    fn a_native_that_may_start_background_work_may_read_it_back() {
        for builtin in all() {
            let rules = effective_rules(&builtin);
            if !is_tool_visible("shell", &rules) {
                continue;
            }
            assert!(
                is_tool_visible("bg", &rules),
                "{} may start a background execution it cannot read back",
                builtin.name
            );
        }
    }

    /// A Job resolves only for the session that delegated it, so only a delegating
    /// native may inspect one.
    #[test]
    fn only_the_delegating_natives_can_inspect_a_durable_job() {
        for builtin in all() {
            let rules = effective_rules(&builtin);
            assert_eq!(
                is_tool_visible("job", &rules),
                DELEGATING_NATIVES.contains(&builtin.name),
                "{} disagrees with the delegation boundary about `job`",
                builtin.name
            );
        }
    }

    /// Only first-party Skills may be forced, and only where the overlay can serve them.
    ///
    /// `required_skills` resolution fails closed on a name that matches nothing, so a
    /// user-installed name here would make the agent unavailable wherever that Skill is
    /// absent. Resolve the first-party descriptor and run the actual visibility gate.
    #[test]
    fn required_native_skills_are_first_party_and_usable_by_their_roles() {
        for builtin in all() {
            let expected = match builtin.name {
                "review" => Some(REVIEW_REQUIRED_SKILLS),
                "deep" => Some(DEEP_REQUIRED_SKILLS),
                _ => None,
            };
            assert_eq!(
                builtin.required_skills(),
                expected,
                "{} must declare its required discipline",
                builtin.name
            );
            let Some(required) = builtin.required_skills() else {
                continue;
            };
            let rules = effective_rules(&builtin);
            assert!(is_tool_visible("skill", &rules));
            for name in required {
                let skill = crate::skill::builtin::skill(name).unwrap_or_else(|| {
                    panic!("{} requires missing first-party Skill {name}", builtin.name)
                });
                assert!(
                    crate::skill::builtin::visible_to(&skill.location, builtin.name, None, &rules),
                    "{} cannot load its required Skill {name}",
                    builtin.name
                );
            }
        }
    }

    #[test]
    fn deep_required_skills_survive_prompt_overrides_and_can_be_configured() {
        use zuno_config::schema::agent::AgentConfig;

        let mut overrides = OrderedMap::new();
        overrides.insert(
            "deep",
            AgentConfig {
                prompt: Some("Investigate the assigned failure.".to_owned()),
                ..AgentConfig::default()
            },
        );
        let resolve_deep = |overrides: &OrderedMap<AgentConfig>| {
            crate::agent::resolve(overrides, &[])
                .into_iter()
                .find(|agent| agent.name == "deep")
                .expect("deep resolves")
        };
        let deep = resolve_deep(&overrides);
        assert_eq!(
            deep.required_skills,
            Some(
                DEEP_REQUIRED_SKILLS
                    .iter()
                    .map(|name| (*name).to_owned())
                    .collect()
            )
        );
        assert_eq!(
            deep.prompt.as_deref(),
            Some("Investigate the assigned failure.")
        );
        overrides.insert(
            "deep",
            AgentConfig {
                required_skills: Some(Vec::new()),
                delegates: Some(vec!["explorer".to_owned()]),
                ..AgentConfig::default()
            },
        );
        let configured = resolve_deep(&overrides);
        assert_eq!(configured.required_skills, Some(Vec::new()));
        assert_eq!(configured.delegates, Some(vec!["explorer".to_owned()]));
    }

    /// The overlay has to actually serve the evidence surface it was written for.
    ///
    /// A deny-by-default overlay hides anything it does not name, so a tool that is added
    /// to the ledger but never added here is dropped by the visibility gate: the agent
    /// would advertise a review workflow it cannot perform, and nothing else would report
    /// it. This test exists because `review_open` and `review_claim` were written after
    /// the overlay and were missing from it.
    ///
    /// The withheld half matters equally. A review that could edit a file could satisfy
    /// its own findings, and one that could close a Goal could clear the gate it exists to
    /// apply.
    #[test]
    fn the_review_overlay_serves_the_whole_evidence_loop_and_withholds_every_writer() {
        let rules = effective_rules(&get("review").expect("review"));

        for tool in [
            "review_open",
            "review_claim",
            "review_get",
            "review_finalize",
            "task_report",
            "council_run",
            "job",
            "goal_get",
        ] {
            assert!(
                is_tool_visible(tool, &rules),
                "review cannot run the evidence loop without `{tool}`"
            );
        }

        for tool in [
            "edit",
            "write",
            "apply_patch",
            "goal_update",
            "goal_propose",
            "plan_update",
            "todo_update",
            "task",
        ] {
            assert!(
                !is_tool_visible(tool, &rules),
                "review is read-only, so `{tool}` must stay withheld"
            );
        }
    }

    /// The two rule layers that decide whether a tool id reaches the model.
    fn effective_rules(builtin: &Builtin) -> Vec<Rule> {
        let mut rules = vec![Rule {
            source: None,
            permission: "*".to_owned(),
            pattern: "*".to_owned(),
            action: PermissionAction::Allow,
        }];
        if let Some(overlay) = builtin.permission_overlay() {
            rules.extend(rules_from_config(&overlay));
        }
        rules
    }

    #[test]
    fn only_the_delegating_natives_declare_and_expose_delegation() {
        let orchestrator = get("orchestrator").expect("orchestrator");
        assert_eq!(orchestrator.delegates, Some(WORK_DELEGATES));
        let deep = get("deep").expect("deep");
        assert_eq!(deep.delegates, Some(WORK_DELEGATES));
        assert!(is_tool_visible("task", &effective_rules(&deep)));
        assert_eq!(
            get("review").expect("review").delegates,
            Some(REVIEW_DELEGATES)
        );
        assert_eq!(
            get("orchestrator")
                .expect("orchestrator")
                .permission_overlay()
                .expect("overlay")
                .rules
                .get("task"),
            Some(&PermissionRule::Action(PermissionAction::Allow))
        );
        let review = get("review")
            .expect("review")
            .permission_overlay()
            .expect("overlay")
            .rules;
        assert_eq!(
            review.get("council_run"),
            Some(&PermissionRule::Action(PermissionAction::Allow))
        );
        assert_ne!(
            review.get("task"),
            Some(&PermissionRule::Action(PermissionAction::Allow))
        );

        for builtin in all()
            .into_iter()
            .filter(|builtin| !DELEGATING_NATIVES.contains(&builtin.name))
        {
            assert!(
                builtin.delegates.is_none(),
                "{} unexpectedly declares child Agents",
                builtin.name
            );
        }
        assert_eq!(
            get("build")
                .expect("build")
                .permission_overlay()
                .expect("overlay")
                .rules
                .get("task"),
            Some(&PermissionRule::Action(PermissionAction::Deny))
        );

        for builtin in all() {
            for target in builtin.delegates.unwrap_or_default() {
                let child = get(target).expect("every native delegate must resolve");
                assert!(matches!(child.mode, AgentMode::Subagent | AgentMode::All));
                assert!(!child.hidden);
                assert_ne!(child.name, "review", "review must never recurse");
            }
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
            "shell",
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
        for denied in ["write", "edit", "patch", "task", "execute"] {
            assert_ne!(
                overlay.get(denied),
                Some(&PermissionRule::Action(PermissionAction::Allow)),
                "Plan mode unexpectedly allows `{denied}`"
            );
        }
    }

    #[test]
    fn primary_modes_and_deep_are_first_class_agents() {
        assert_eq!(
            get("orchestrator").expect("orchestrator").mode,
            AgentMode::Primary
        );
        assert_eq!(get("build").expect("build").mode, AgentMode::Primary);
        assert_eq!(get("plan").expect("plan").mode, AgentMode::Primary);
        assert_eq!(get("deep").expect("deep").mode, AgentMode::All);
        let deep = get("deep")
            .expect("deep")
            .permission_overlay()
            .expect("deep permissions")
            .rules;
        for capability in [
            "question",
            "goal_get",
            "goal_propose",
            "goal_update",
            "goal_request_input",
            "task",
            "job",
        ] {
            assert_eq!(
                deep.get(capability),
                Some(&PermissionRule::Action(PermissionAction::Allow)),
                "direct deep work must be able to own `{capability}`"
            );
        }
    }

    const VERIFICATION_SECTION: &str = "\n\n## Verification\n";

    #[test]
    fn task_callers_receive_the_flat_contract_and_intent_distinction() {
        for prompt in [PROMPT_ORCHESTRATOR, PROMPT_DEEP] {
            for field in [
                "`agent`",
                "`objective`",
                "`deliverable`",
                "`instructions`",
                "`success_evidence`",
            ] {
                assert!(prompt.contains(field), "missing {field}");
            }
            assert!(prompt.contains("top level"));
            assert!(prompt.contains("`intent` never substitutes"));
        }
        assert!(
            !PROMPT_BUILD.contains("`success_evidence`"),
            "do not teach delegation to direct build"
        );
        assert!(
            !PROMPT_REVIEW.contains("`success_evidence`"),
            "review uses Council instead of task"
        );
    }

    #[test]
    fn serial_wait_guidance_does_not_detach_to_end_the_turn() {
        let rubric = verification_rubric(PROMPT_ORCHESTRATOR);
        assert!(rubric.contains("waiting is the only useful next action"));
        assert!(rubric.contains("do not detach merely to end the turn"));
        assert!(rubric.contains("same handle"));
        assert!(rubric.contains("steering/interruption"));
        assert!(rubric.contains("requires an explicit parallel split"));
        assert!(
            rubric.contains("identify the independent work you will do locally before dispatch")
        );
        assert!(rubric.contains("Do not background the sole task and finalize while it runs"));
    }

    fn verification_rubric(prompt: &str) -> &str {
        prompt
            .split_once(VERIFICATION_SECTION)
            .expect("the assembled working prompt must include shared verification guidance")
            .1
    }

    fn role_prompt_words(prompt: &str) -> usize {
        prompt
            .split_once(VERIFICATION_SECTION)
            .map_or(prompt, |(role, _)| role)
            .split_whitespace()
            .count()
    }

    // These assertions exercise the catalog's rendered prompt contract. They do
    // not prove that a model followed the instructions or that runtime behavior
    // is correct merely because its source contains particular strings.
    #[test]
    fn verification_rubric_is_shared_once_by_working_agents_only() {
        let shared = verification_rubric(PROMPT_BUILD);
        assert!(
            shared.split_whitespace().count() <= 320,
            "keep the common rubric compact independently of each role's word budget"
        );

        for agent in crate::agent::resolve(&OrderedMap::new(), &[]) {
            let prompt = agent.prompt.as_deref().expect("a native prompt");
            if agent.hidden == Some(true) {
                assert!(
                    !prompt.contains(VERIFICATION_SECTION),
                    "{} must keep its tool-free output contract",
                    agent.name
                );
                continue;
            }
            assert_eq!(
                prompt.matches(VERIFICATION_SECTION).count(),
                1,
                "{} must receive exactly one verification rubric",
                agent.name
            );
            assert_eq!(
                verification_rubric(prompt),
                shared,
                "{} must receive the same rubric as the writing and testing roles",
                agent.name
            );
        }
    }

    #[test]
    fn verification_rubric_orders_behavior_red_before_implementation_and_green() {
        let rubric = verification_rubric(PROMPT_BUILD);
        let phases = [
            "fixing a reproducible bug or changing a state machine",
            "first add or extend a focused behavior test",
            "against the old implementation",
            "Confirm it fails for the target behavior",
            "Then implement the change",
            "rerun that test to green",
            "relevant regression checks",
        ];
        let mut previous = 0;
        for phase in phases {
            let position = rubric
                .find(phase)
                .unwrap_or_else(|| panic!("missing verification phase: {phase}"));
            assert!(position >= previous, "{phase} is out of order");
            previous = position + phase.len();
        }
        for clause in [
            "build, dependency, permission, or environment error is not a red regression",
            "observable inputs, outputs, transitions",
            "interruption, restart, or recovery",
        ] {
            assert!(rubric.contains(clause), "missing behavior scope: {clause}");
        }
    }

    #[test]
    fn verification_rubric_scopes_read_only_evidence_and_requires_checkable_receipts() {
        let rubric = verification_rubric(PROMPT_REVIEW);
        for clause in [
            "when writing, fixing, testing, or reviewing",
            "Read-only roles collect existing reproduction steps, tests, and receipts",
            "without editing files or running commands that write",
            "hand missing tests to an authorized writer",
            "Reviewers check the red/green evidence",
            "a plan identifies the test and expected failure without claiming it ran",
            "exact commands",
            "working directory",
            "tested source/input identity",
            "expected versus observed results",
            "exit status",
            "authoritative test output or artifact/run receipts",
            "completed checks from proposed, blocked, and unrun checks",
        ] {
            assert!(
                rubric.contains(clause),
                "missing evidence boundary: {clause}"
            );
        }
    }

    #[test]
    fn verification_rubric_keeps_checks_proportional_and_serial_waits_in_foreground() {
        let rubric = verification_rubric(PROMPT_ORCHESTRATOR);
        for clause in [
            "If reproduction is unavailable, explain the limitation",
            "proportional checks for documentation, trivial changes, and command/script deliverables",
            "do not manufacture tests for every command",
            "Source-string assertions alone do not establish runtime behavior",
            "prompt-output contract tests establish only the rendered prompt contract",
            "serial CI waits in the same foreground workflow",
            "Background work requires an explicit parallel split",
            "A polling timeout is not remote failure",
            "inspect the authoritative run status",
            "no tool authority, runtime gate, or approval requirement",
        ] {
            assert!(rubric.contains(clause), "missing proportionality: {clause}");
        }
    }

    #[test]
    fn verification_rubric_does_not_override_configured_prompts() {
        use zuno_config::schema::agent::AgentConfig;

        let mut overrides = OrderedMap::new();
        for name in ["build", "review", "fixer", "custom-test"] {
            overrides.insert(
                name,
                AgentConfig {
                    prompt: Some("Use the supplied acceptance criteria.".to_owned()),
                    ..AgentConfig::default()
                },
            );
        }
        for agent in crate::agent::resolve(&overrides, &[])
            .into_iter()
            .filter(|agent| overrides.contains_key(&agent.name))
        {
            assert_eq!(
                agent.prompt.as_deref(),
                Some("Use the supplied acceptance criteria."),
                "{} must preserve an explicit prompt override",
                agent.name
            );
        }
    }

    #[test]
    fn delivery_prompts_require_evidence_without_becoming_policy_dumps() {
        let cases: [(&str, &str, usize, &[&str]); 4] = [
            (
                "orchestrator",
                PROMPT_ORCHESTRATOR,
                225,
                &[
                    "dependency graph",
                    "non-overlapping objectives",
                    "Treat child reports as evidence",
                    "integration ownership",
                    "Batch independent reads and checks",
                    "Do not re-read unchanged state",
                    "Run shared verification once",
                    "After a second failure at one integration boundary",
                    "producer-artifact-consumer contract",
                    "Keep one polling owner",
                ],
            ),
            (
                "build",
                PROMPT_BUILD,
                170,
                &[
                    "direct implementation owner",
                    "owning abstraction",
                    "Do not delegate",
                    "affected callers",
                ],
            ),
            (
                "plan",
                PROMPT_PLAN,
                150,
                &[
                    "read-only planning",
                    "Explore facts first",
                    "Honor an explicit inspection scope",
                    "material choices",
                    "decision-complete",
                    "Do not invent APIs",
                    "defer non-blocking choices",
                    "durable Plan and Todo state",
                ],
            ),
            (
                "deep",
                PROMPT_DEEP,
                240,
                &[
                    "difficult debugging",
                    "Reproduce the failure",
                    "Rank competing hypotheses",
                    "discriminating experiment",
                    "revise the hypotheses",
                    "causal chain",
                    "root fix",
                    "recovery path",
                    "runtime tools, permissions, and depth",
                    "independent verification",
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
            let words = role_prompt_words(prompt);
            assert!(
                words <= word_limit,
                "{name} prompt grew to {words} words; concise role policy belongs here, not a \
                 second harness manual"
            );
        }
    }

    #[test]
    fn writing_roles_preserve_analysis_intent_and_authorized_command_work() {
        for prompt in [
            PROMPT_ORCHESTRATOR,
            PROMPT_BUILD,
            PROMPT_DEEP,
            PROMPT_FIXER,
            PROMPT_GENERAL,
        ] {
            assert!(prompt.contains("explain-only or diagnosis-only"));
            assert!(prompt.contains("authorized") || prompt.contains("Authorized"));
            assert!(prompt.contains("command") || prompt.contains("Commands"));
            assert!(prompt.contains("script"));
        }
    }

    #[test]
    fn role_prompts_do_not_duplicate_runtime_execution_policy() {
        for (name, prompt) in [
            ("orchestrator", PROMPT_ORCHESTRATOR),
            ("build", PROMPT_BUILD),
            ("deep", PROMPT_DEEP),
            ("fixer", PROMPT_FIXER),
            ("general", PROMPT_GENERAL),
        ] {
            for duplicated in [
                "`plan_update`",
                "`git apply --check`",
                "`git reset --hard`",
                "`git checkout --`",
                "Git metadata is not the freshness authority",
            ] {
                assert!(
                    !prompt.contains(duplicated),
                    "{name} prompt duplicates runtime or Skill policy `{duplicated}`:\n{prompt}"
                );
            }
        }
    }

    #[test]
    fn specialist_prompts_define_evidence_output_and_scope_boundaries() {
        let cases: [(&str, &str, usize, &[&str]); 6] = [
            (
                "explorer",
                PROMPT_EXPLORER,
                145,
                &[
                    "actual runtime path",
                    "what the code proves",
                    "External sources",
                ],
            ),
            (
                "librarian",
                PROMPT_LIBRARIAN,
                145,
                &["exact version", "primary sources", "may drift over time"],
            ),
            (
                "oracle",
                PROMPT_ORACLE,
                170,
                &[
                    "ownership boundaries",
                    "demonstrated defects",
                    "another Agent implements",
                ],
            ),
            (
                "fixer",
                PROMPT_FIXER,
                145,
                &[
                    "smallest sufficient change",
                    "local regression",
                    "return it to the parent",
                ],
            ),
            (
                "general",
                PROMPT_GENERAL,
                150,
                &[
                    "explicit deliverable",
                    "scope envelope",
                    "architecture decisions",
                ],
            ),
            (
                "looker",
                PROMPT_LOOKER,
                145,
                &["full artifact", "direct observation", "missing frames"],
            ),
        ];

        for (name, prompt, word_limit, clauses) in cases {
            for clause in clauses {
                assert!(
                    prompt.contains(clause),
                    "{name} prompt is missing `{clause}`:\n{prompt}"
                );
            }
            let words = role_prompt_words(prompt);
            assert!(
                words <= word_limit,
                "{name} prompt grew to {words} words; keep role guidance compact"
            );
            for heading in ["Outcome", "Evidence", "Inspected/Changed", "Risks/Blocker"] {
                assert!(
                    prompt.contains(heading),
                    "{name} prompt is missing the shared `{heading}` report contract"
                );
            }
            assert!(prompt.contains("Do not emit JSON or XML"), "{prompt}");
        }
    }
}

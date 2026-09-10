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

macro_rules! specialist_prompt {
    ($path:literal) => {
        concat!(
            include_str!($path),
            "\n\nReturn concise natural Markdown. Use these headings when they add value: \
             Outcome, Evidence, Inspected/Changed, Risks/Blocker. Omit empty headings. Do not \
             emit JSON or XML unless the caller explicitly requires machine-readable output."
        )
    };
}

/// Default multi-agent coordinator.
pub const PROMPT_ORCHESTRATOR: &str = include_str!("prompt/orchestrator.txt");
/// Direct end-to-end implementation agent.
pub const PROMPT_BUILD: &str = include_str!("prompt/build.txt");
/// Read-only planning agent.
pub const PROMPT_PLAN: &str = include_str!("prompt/plan.txt");
/// Read-only high-assurance review agent.
pub const PROMPT_REVIEW: &str = include_str!("prompt/review.txt");
/// Thorough cross-cutting implementation agent.
pub const PROMPT_DEEP: &str = include_str!("prompt/deep.txt");
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

const ORCHESTRATOR_DELEGATES: &[&str] = &[
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
/// `orchestrator` coordinates delivery and owns integration; `review` seats the
/// first-party `balanced-review` Council. Nothing else declares children, and `build`
/// denies `task` outright, because an agent that can fan out without owning the
/// verification of what comes back is how one review turn came to run three
/// same-shaped explorers and inherit a wrong conclusion from all of them.
///
/// One list so the catalog and its tests cannot disagree about where the boundary is.
pub const DELEGATING_NATIVES: [&str; 2] = ["orchestrator", "review"];

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
        delegates: Some(ORCHESTRATOR_DELEGATES),
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
             decides whether it is ready to implement, without changing any file.",
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
            "Runs difficult debugging and cross-cutting implementation either as the selected \
             session Agent or as one bounded delegated objective, without spawning children.",
        ),
        mode: AgentMode::All,
        hidden: false,
        temperature: Some(0.1),
        prompt: Some(PROMPT_DEEP),
        delegates: None,
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
    /// denies delegation. `deep` is directly selectable and delegable, has no child
    /// tools, and may own the same durable Goal lifecycle as a primary writer. `plan`
    /// may inspect and write only its plan document; the path-specific edit grants are
    /// added by the CLI composition root.
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
            rules.push(("report_write", allow()));
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

    /// Whether the composition root must add runtime path rules.
    #[must_use]
    pub fn permission_overlay_is_partial(&self) -> bool {
        matches!(
            self.name,
            "plan" | "review" | "explorer" | "librarian" | "oracle" | "looker"
        )
    }

    /// Skills this native loads at the start of every turn.
    ///
    /// `None` leaves Skill selection to the model, which is right for an agent whose work
    /// is not defined by one discipline. `review` is the exception: its whole job is to
    /// produce checkable evidence, and a review that forgot to load the evidence
    /// discipline is the failure the agent exists to prevent.
    ///
    /// Only first-party names may appear here; see [`REVIEW_REQUIRED_SKILLS`].
    #[must_use]
    pub fn required_skills(&self) -> Option<&'static [&'static str]> {
        match self.name {
            "review" => Some(REVIEW_REQUIRED_SKILLS),
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
    fn delegable_agents_are_deny_by_default_and_cannot_delegate() {
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
            assert!(
                !overlay.iter().any(|(key, rule)| {
                    key == "task" && rule == &PermissionRule::Action(PermissionAction::Allow)
                }),
                "{} must not create grandchildren",
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
    /// absent. The `codegraph` assertion is the specific mistake this guards.
    #[test]
    fn only_review_forces_skills_and_only_first_party_ones() {
        for builtin in all() {
            if builtin.name == "review" {
                assert_eq!(
                    builtin.required_skills(),
                    Some(REVIEW_REQUIRED_SKILLS),
                    "review must load the evidence discipline it exists to apply"
                );
            } else {
                assert!(
                    builtin.required_skills().is_none(),
                    "{} unexpectedly forces a Skill load",
                    builtin.name
                );
            }
        }
        assert!(
            !REVIEW_REQUIRED_SKILLS.contains(&"codegraph"),
            "`codegraph` is user-installed; requiring it would fail closed where it is absent"
        );
        let rules = effective_rules(&get("review").expect("review"));
        for tool in ["read", "glob", "grep", "skill"] {
            assert!(
                is_tool_visible(tool, &rules),
                "review forces Skills that need `{tool}`, so its overlay must grant it"
            );
        }
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
        assert_eq!(orchestrator.delegates, Some(ORCHESTRATOR_DELEGATES));
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
        ] {
            assert_eq!(
                deep.get(capability),
                Some(&PermissionRule::Action(PermissionAction::Allow)),
                "direct deep work must be able to own `{capability}`"
            );
        }
        assert_ne!(
            deep.get("task"),
            Some(&PermissionRule::Action(PermissionAction::Allow)),
            "Goal ownership must not grant recursive delegation"
        );
    }

    #[test]
    fn delivery_prompts_require_evidence_without_becoming_policy_dumps() {
        let cases: [(&str, &str, usize, &[&str]); 4] = [
            (
                "orchestrator",
                PROMPT_ORCHESTRATOR,
                190,
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
                    "one authoritative observer",
                ],
            ),
            (
                "build",
                PROMPT_BUILD,
                130,
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
                ],
            ),
            (
                "deep",
                PROMPT_DEEP,
                170,
                &[
                    "difficult debugging",
                    "Reproduce the failure",
                    "Rank competing hypotheses",
                    "causal chain",
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
            let words = prompt.split_whitespace().count();
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

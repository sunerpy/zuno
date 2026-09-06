//! Durable planning policy shared by every client surface.
//!
//! The host owns explicit Plan mode and existing durable state. In ordinary Work
//! mode it does not infer task complexity from prompt vocabulary: the model decides
//! whether an optional strategic Plan adds coordination or recovery value.

/// Facts available before a turn reaches the provider.
#[derive(Debug, Clone, Copy)]
pub struct PlanningInput<'a> {
    prompt: &'a str,
    mode: PlanningMode,
    source: PlanningInputSource,
    existing_plan: ExistingPlanState,
    content: PlanningContentFacts,
    plan_available: bool,
}

impl<'a> PlanningInput<'a> {
    /// Build an input for an Agent with no existing durable plan.
    #[must_use]
    pub fn new(prompt: &'a str, agent: &str) -> Self {
        Self {
            prompt,
            mode: PlanningMode::from_agent(agent),
            source: PlanningInputSource::User,
            existing_plan: ExistingPlanState::None,
            content: PlanningContentFacts::empty(),
            plan_available: true,
        }
    }

    /// Record which durable input path produced this turn.
    #[must_use]
    pub const fn with_source(mut self, source: PlanningInputSource) -> Self {
        self.source = source;
        self
    }

    /// Record the durable plan state before this input.
    #[must_use]
    pub const fn with_existing_plan(mut self, existing: ExistingPlanState) -> Self {
        self.existing_plan = existing;
        self
    }

    /// Record non-text context supplied alongside the user text.
    #[must_use]
    pub const fn with_content(mut self, content: PlanningContentFacts) -> Self {
        self.content = content;
        self
    }

    /// Record whether the effective tool surface can maintain a durable plan.
    #[must_use]
    pub const fn with_plan_available(mut self, available: bool) -> Self {
        self.plan_available = available;
        self
    }
}

/// Collaboration mode relevant to durable planning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanningMode {
    Work,
    Plan,
}

impl PlanningMode {
    fn from_agent(agent: &str) -> Self {
        if agent == "plan" {
            Self::Plan
        } else {
            Self::Work
        }
    }
}

/// Durable origin of the input being classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanningInputSource {
    /// A user-authored prompt, including a promoted TUI or ACP input.
    User,
    /// A user-invoked command or Skill whose resolved prompt is now executing.
    Command,
    /// A user-created or materially edited durable Goal objective.
    GoalObjective,
    /// A settled child job report admitted into the parent inbox.
    ChildReport,
    /// A terminal process-owned background execution admitted into the parent inbox.
    BackgroundReport,
    /// Mid-turn steering already attached to an active execution.
    Steering,
    /// A host-generated retry or recovery continuation.
    Retry,
}

impl PlanningInputSource {
    const fn may_create_plan(self) -> bool {
        matches!(self, Self::User | Self::Command | Self::GoalObjective)
    }
}

/// State of the one durable plan row associated with a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingPlanState {
    None,
    /// At least one step remains pending or in progress.
    Active,
    /// Every existing step completed; a new user objective may replace the visible Plan.
    Terminal,
}

/// Bounded structural facts about typed user content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanningContentFacts {
    contextual_blocks: usize,
    text_blocks: usize,
    total_bytes: usize,
    branch_or_selection_context: bool,
}

impl PlanningContentFacts {
    /// No typed content beyond the ordinary prompt string.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            contextual_blocks: 0,
            text_blocks: 0,
            total_bytes: 0,
            branch_or_selection_context: false,
        }
    }

    /// Build facts from a client surface without exposing provider block types here.
    #[must_use]
    pub const fn new(
        contextual_blocks: usize,
        text_blocks: usize,
        total_bytes: usize,
        branch_or_selection_context: bool,
    ) -> Self {
        Self {
            contextual_blocks,
            text_blocks,
            total_bytes,
            branch_or_selection_context,
        }
    }

    const fn benefits_from_plan(self) -> bool {
        self.branch_or_selection_context
            || self.contextual_blocks > 0
            || (self.text_blocks > 1 && self.total_bytes >= 4 * 1024)
    }
}

/// Why the host chose one planning path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanningRationale {
    code: &'static str,
    message: &'static str,
}

impl PlanningRationale {
    /// Stable machine-readable reason.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }

    /// Concise model- and user-facing explanation.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        self.message
    }
}

/// Host planning outcome for one input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanningDecision {
    /// Explicit Plan mode must create or replace a strategic Plan.
    Required(PlanningRationale),
    /// Keep the existing plan current; never replace it with a generic seed.
    Maintain(PlanningRationale),
    /// Work mode exposes Plan as an optional model decision.
    Optional(PlanningRationale),
    /// No user objective exists for which a new Plan could be useful.
    Atomic(PlanningRationale),
    /// The effective Agent cannot maintain a plan.
    Unavailable(PlanningRationale),
}

impl PlanningDecision {
    /// The rationale attached to every decision.
    #[must_use]
    pub const fn rationale(&self) -> &PlanningRationale {
        match self {
            Self::Required(reason)
            | Self::Maintain(reason)
            | Self::Optional(reason)
            | Self::Atomic(reason)
            | Self::Unavailable(reason) => reason,
        }
    }

    /// Relationship between the user-visible Plan and dynamic Todo detail.
    #[must_use]
    pub const fn guidance(&self) -> &'static str {
        "Plan steps are strategic, user-visible outcomes. Todo items are optional concrete work \
         beneath plan steps and should be used only when finer ownership, dependency, or recovery \
         tracking helps."
    }
}

/// Deterministic state policy shared by CLI, TUI, ACP, and server turns.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlanningPolicy;

impl PlanningPolicy {
    /// Select the planning contract before the first provider call.
    #[must_use]
    pub fn classify(input: PlanningInput<'_>) -> PlanningDecision {
        if !input.plan_available {
            return PlanningDecision::Unavailable(reason(
                "plan_unavailable",
                "the effective Agent cannot update durable plans",
            ));
        }
        if input.mode == PlanningMode::Plan {
            if input.existing_plan == ExistingPlanState::Active {
                return PlanningDecision::Maintain(reason(
                    "explicit_plan_mode_active",
                    "explicit Plan mode must keep the durable Plan current",
                ));
            }
            return PlanningDecision::Required(reason(
                "explicit_plan_mode",
                "explicit Plan mode must produce a durable strategic Plan",
            ));
        }
        if !input.source.may_create_plan() {
            if input.existing_plan == ExistingPlanState::Active {
                return PlanningDecision::Maintain(reason(
                    "active_plan_continuation",
                    "host-generated continuations and reports remain attached to the active plan",
                ));
            }
            return PlanningDecision::Atomic(reason(
                "non_user_input",
                "host-generated continuations and child reports do not create a new plan",
            ));
        }
        if input.existing_plan == ExistingPlanState::Active {
            return PlanningDecision::Maintain(reason(
                "active_plan_reconciliation",
                "the model must keep, replace, or supersede the active Plan for the current objective",
            ));
        }
        if input.prompt.trim().is_empty() {
            return PlanningDecision::Atomic(reason(
                "empty_input",
                "an empty input does not create a durable Plan",
            ));
        }
        PlanningDecision::Optional(reason(
            if input.content.benefits_from_plan() {
                "structured_context_advisory"
            } else if input.source == PlanningInputSource::GoalObjective {
                "goal_owned_execution"
            } else if input.existing_plan == ExistingPlanState::Terminal {
                "terminal_plan_available"
            } else {
                "model_decides"
            },
            "Work mode lets the model use a Plan only when the task is meaningfully multi-step",
        ))
    }
}

const fn reason(code: &'static str, message: &'static str) -> PlanningRationale {
    PlanningRationale { code, message }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_mode_leaves_plan_choice_to_the_model_without_parsing_prompt_vocabulary() {
        for prompt in [
            "OK  你修改下吧",
            "Update this config value to 872000.",
            "Investigate the shell bug, implement the fix, and run the focused tests.",
            "修复跨 crate 的调用链，并完成测试、clippy 和构建验收。",
            "Why does this error happen?",
            "hi",
        ] {
            let decision = PlanningPolicy::classify(PlanningInput::new(prompt, "orchestrator"));
            assert!(
                matches!(decision, PlanningDecision::Optional(_)),
                "{prompt:?} must not be forced into a Plan by a host-side word list: {decision:?}"
            );
            assert_eq!(decision.rationale().code(), "model_decides");
        }
    }

    #[test]
    fn explicit_plan_mode_requires_durable_plan_state() {
        let decision =
            PlanningPolicy::classify(PlanningInput::new("Design the migration.", "plan"));

        assert!(matches!(decision, PlanningDecision::Required(_)));
        assert_eq!(decision.rationale().code(), "explicit_plan_mode");
    }

    #[test]
    fn an_active_plan_is_reconciled_without_guessing_user_intent_from_text() {
        for prompt in [
            "继续按计划处理。",
            "另外，深入定位 GitHub Actions 失败并修复发布链路。",
            "OK  你修改下吧",
        ] {
            let decision = PlanningPolicy::classify(
                PlanningInput::new(prompt, "orchestrator")
                    .with_existing_plan(ExistingPlanState::Active),
            );
            assert!(
                matches!(decision, PlanningDecision::Maintain(_)),
                "{prompt:?} must let the model maintain, replace, or supersede active state"
            );
            assert_eq!(decision.rationale().code(), "active_plan_reconciliation");
        }
    }

    #[test]
    fn explicit_plan_mode_maintains_an_existing_plan() {
        let decision = PlanningPolicy::classify(
            PlanningInput::new("Refine the migration design.", "plan")
                .with_existing_plan(ExistingPlanState::Active),
        );

        assert!(matches!(decision, PlanningDecision::Maintain(_)));
        assert_eq!(decision.rationale().code(), "explicit_plan_mode_active");
    }

    #[test]
    fn host_generated_input_never_opens_a_new_plan() {
        for source in [
            PlanningInputSource::ChildReport,
            PlanningInputSource::BackgroundReport,
            PlanningInputSource::Steering,
            PlanningInputSource::Retry,
        ] {
            let decision = PlanningPolicy::classify(
                PlanningInput::new("Implemented and verified the change.", "orchestrator")
                    .with_source(source),
            );
            assert!(matches!(decision, PlanningDecision::Atomic(_)));
            assert_eq!(decision.rationale().code(), "non_user_input");
        }
    }

    #[test]
    fn host_generated_input_keeps_active_plan_state_authoritative() {
        let decision = PlanningPolicy::classify(
            PlanningInput::new("Child result.", "orchestrator")
                .with_source(PlanningInputSource::ChildReport)
                .with_existing_plan(ExistingPlanState::Active),
        );

        assert!(matches!(decision, PlanningDecision::Maintain(_)));
        assert_eq!(decision.rationale().code(), "active_plan_continuation");
    }

    #[test]
    fn typed_context_is_advisory_in_work_mode_not_a_host_forced_plan() {
        let decision = PlanningPolicy::classify(
            PlanningInput::new("Apply the selected change.", "build")
                .with_content(PlanningContentFacts::new(1, 1, 32_000, true)),
        );

        assert!(matches!(decision, PlanningDecision::Optional(_)));
        assert_eq!(decision.rationale().code(), "structured_context_advisory");
    }

    #[test]
    fn a_goal_is_already_durable_and_does_not_implicitly_require_a_plan() {
        let decision = PlanningPolicy::classify(
            PlanningInput::new("Finish the requested configuration change.", "build")
                .with_source(PlanningInputSource::GoalObjective),
        );

        assert!(matches!(decision, PlanningDecision::Optional(_)));
        assert_eq!(decision.rationale().code(), "goal_owned_execution");
    }

    #[test]
    fn a_terminal_plan_does_not_force_the_next_work_objective_to_replace_it() {
        let decision = PlanningPolicy::classify(
            PlanningInput::new("Fix the next issue.", "build")
                .with_existing_plan(ExistingPlanState::Terminal),
        );

        assert!(matches!(decision, PlanningDecision::Optional(_)));
        assert_eq!(decision.rationale().code(), "terminal_plan_available");
    }

    #[test]
    fn empty_input_and_hidden_plan_tool_have_explicit_non_planning_outcomes() {
        let empty = PlanningPolicy::classify(PlanningInput::new("  ", "build"));
        assert!(matches!(empty, PlanningDecision::Atomic(_)));
        assert_eq!(empty.rationale().code(), "empty_input");

        let unavailable = PlanningPolicy::classify(
            PlanningInput::new("Implement the change.", "build").with_plan_available(false),
        );
        assert!(matches!(unavailable, PlanningDecision::Unavailable(_)));
        assert_eq!(unavailable.rationale().code(), "plan_unavailable");
    }

    #[test]
    fn todo_is_optional_detail_beneath_plan_steps() {
        let decision = PlanningPolicy::classify(PlanningInput::new(
            "Investigate, implement, and verify the runtime change.",
            "build",
        ));

        assert!(
            decision
                .guidance()
                .contains("Todo items are optional concrete work beneath plan steps")
        );
        assert!(!decision.guidance().contains("one Todo per Plan step"));
    }
}

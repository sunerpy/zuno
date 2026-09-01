//! Host-owned durable planning policy.
//!
//! The model may refine a plan, but it does not decide whether ordinary
//! engineering work receives one. That decision is made here from stable request
//! facts before the first provider call, so every client surface behaves the same.

/// Facts available before a turn reaches the provider.
#[derive(Debug, Clone, Copy)]
pub struct PlanningInput<'a> {
    prompt: &'a str,
    agent: &'a str,
    source: PlanningInputSource,
    existing_plan: ExistingPlanState,
    content: PlanningContentFacts,
    plan_available: bool,
}

impl<'a> PlanningInput<'a> {
    /// Build an input for a plan-capable Agent with no existing durable plan.
    #[must_use]
    pub const fn new(prompt: &'a str, agent: &'a str) -> Self {
        Self {
            prompt,
            agent,
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

/// One host-seeded plan step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanningStepSeed {
    id: &'static str,
    title: &'static str,
}

impl PlanningStepSeed {
    /// Stable step identity that a later model update must preserve.
    #[must_use]
    pub const fn id(&self) -> &'static str {
        self.id
    }

    /// Initial human-readable step title.
    #[must_use]
    pub const fn title(&self) -> &'static str {
        self.title
    }
}

/// Initial durable plan created by the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanningSeed {
    title: String,
    steps: Vec<PlanningStepSeed>,
    rationale: PlanningRationale,
}

impl PlanningSeed {
    /// Bounded title derived from the request.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Stable initial roadmap.
    #[must_use]
    pub fn steps(&self) -> &[PlanningStepSeed] {
        &self.steps
    }

    /// Why the host created this plan.
    #[must_use]
    pub const fn rationale(&self) -> &PlanningRationale {
        &self.rationale
    }

    /// Relationship between the durable Plan and optional Todo detail.
    #[must_use]
    pub const fn guidance(&self) -> &'static str {
        "Todo items are optional concrete work beneath plan steps. Use them when finer ownership, \
         dependency, or recovery tracking helps; avoid a mechanical one-to-one mapping."
    }
}

/// Host planning outcome for one input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanningDecision {
    /// Create a new durable plan before the provider sees the request.
    Create(PlanningSeed),
    /// Keep the existing plan current; never replace it with a generic seed.
    Maintain(PlanningRationale),
    /// A direct answer, single read, or genuinely atomic operation may proceed directly.
    Atomic(PlanningRationale),
    /// The effective Agent cannot maintain a plan.
    Unavailable(PlanningRationale),
}

impl PlanningDecision {
    /// The new plan seed, when one must be created.
    #[must_use]
    pub const fn seed(&self) -> Option<&PlanningSeed> {
        match self {
            Self::Create(seed) => Some(seed),
            Self::Maintain(_) | Self::Atomic(_) | Self::Unavailable(_) => None,
        }
    }

    /// The rationale attached to every decision.
    #[must_use]
    pub const fn rationale(&self) -> &PlanningRationale {
        match self {
            Self::Create(seed) => seed.rationale(),
            Self::Maintain(reason) | Self::Atomic(reason) | Self::Unavailable(reason) => reason,
        }
    }
}

/// Deterministic host policy shared by CLI, TUI, ACP, and server turns.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlanningPolicy;

impl PlanningPolicy {
    /// Classify one request before its first provider call.
    #[must_use]
    pub fn classify(input: PlanningInput<'_>) -> PlanningDecision {
        if !input.plan_available {
            return PlanningDecision::Unavailable(reason(
                "plan_unavailable",
                "the effective Agent cannot update durable plans",
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

        let prompt = input.prompt.trim();
        if prompt.is_empty() {
            if input.existing_plan == ExistingPlanState::Active {
                return PlanningDecision::Maintain(reason(
                    "active_plan_continuation",
                    "an empty continuation does not replace the active plan",
                ));
            }
            return PlanningDecision::Atomic(reason(
                "direct_answer",
                "a direct answer does not benefit from durable execution state",
            ));
        }
        if input.existing_plan == ExistingPlanState::Active
            && input.source != PlanningInputSource::GoalObjective
            && !input.content.benefits_from_plan()
            && (direct_answer(prompt)
                || single_read(prompt)
                || atomic_commit(prompt)
                || active_plan_continuation(prompt))
        {
            return PlanningDecision::Maintain(reason(
                "active_plan_continuation",
                "a bounded answer, action, or explicit continuation keeps the active plan current",
            ));
        }
        if input.content.benefits_from_plan() {
            return create_plan(
                input,
                if input.source == PlanningInputSource::GoalObjective {
                    "goal_objective_replaced"
                } else if input.existing_plan == ExistingPlanState::Active {
                    "active_plan_replaced"
                } else {
                    "typed_context"
                },
            );
        }
        if direct_answer(prompt) {
            return PlanningDecision::Atomic(reason(
                "direct_answer",
                "a direct answer does not benefit from durable execution state",
            ));
        }
        if single_read(prompt) {
            return PlanningDecision::Atomic(reason(
                "single_read",
                "one bounded read is a genuinely atomic operation",
            ));
        }
        if atomic_commit(prompt) {
            return PlanningDecision::Atomic(reason(
                "atomic_commit",
                "one bounded commit of already-prepared changes is atomic",
            ));
        }

        create_plan(
            input,
            if input.existing_plan == ExistingPlanState::Active
                && input.source != PlanningInputSource::GoalObjective
            {
                "active_plan_replaced"
            } else if input.source == PlanningInputSource::GoalObjective {
                "goal_objective_replaced"
            } else if input.existing_plan == ExistingPlanState::Terminal {
                "terminal_plan_replaced"
            } else {
                "durable_plan_required"
            },
        )
    }
}

fn create_plan(input: PlanningInput<'_>, code: &'static str) -> PlanningDecision {
    PlanningDecision::Create(PlanningSeed {
        title: plan_title(input.prompt),
        steps: plan_steps(input.agent),
        rationale: reason(
            code,
            "the request benefits from progress visibility, dependency management, recovery, or verification",
        ),
    })
}

const fn reason(code: &'static str, message: &'static str) -> PlanningRationale {
    PlanningRationale { code, message }
}

fn normalized(prompt: &str) -> String {
    prompt.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn direct_answer(prompt: &str) -> bool {
    let lower = normalized(prompt).to_lowercase();
    let question = prompt.ends_with('?')
        || prompt.ends_with('？')
        || [
            "why ",
            "what ",
            "how ",
            "explain ",
            "describe ",
            "is ",
            "are ",
            "can ",
            "does ",
            "为什么",
            "是什么",
            "如何",
            "怎么",
            "是否",
            "能否",
            "请解释",
            "说明一下",
        ]
        .iter()
        .any(|prefix| lower.starts_with(prefix));
    question && !contains_engineering_action(&lower)
}

fn single_read(prompt: &str) -> bool {
    let lower = normalized(prompt).to_lowercase();
    let starts_with_read = [
        "read ", "show ", "display ", "open ", "读取", "查看", "展示", "打开",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix));
    starts_with_read && !multi_stage(prompt) && action_count(prompt) <= 1
}

fn atomic_commit(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    let words = english_words(&lower);
    let commit = words.contains(&"commit") || lower.contains("提交");
    commit
        && !has_stage_separator(prompt)
        && ![
            "push", "推送", "test", "测试", "build", "构建", "release", "发布",
        ]
        .iter()
        .any(|signal| {
            if signal.is_ascii() {
                words.contains(signal)
            } else {
                lower.contains(signal)
            }
        })
        && lower.chars().count() <= 96
}

fn active_plan_continuation(prompt: &str) -> bool {
    let lower = normalized(prompt).to_lowercase();
    let has_new_boundary = [
        "另外",
        "另一个",
        "新增",
        "改为",
        "同时还",
        "还需要",
        "but first",
        "however",
        "instead",
        "another ",
        "new objective",
    ]
    .iter()
    .any(|boundary| lower.contains(boundary));
    let explicit_continuation = [
        "continue",
        "continue ",
        "proceed",
        "proceed ",
        "go ahead",
        "go ahead ",
        "resume",
        "resume ",
        "keep going",
        "keep going ",
        "ok",
        "okay",
        "yes",
        "sure",
        "继续",
        "继续 ",
        "接着",
        "接着 ",
        "按计划",
        "按你的建议",
        "按你的分析",
        "好的",
        "好",
        "可以",
        "没问题",
        "已配置",
        "已完成",
    ]
    .iter()
    .any(|prefix| lower == *prefix || lower.starts_with(prefix));
    let bounded_constraint = lower.chars().count() <= 160
        && action_count(prompt) == 0
        && !multi_stage(prompt)
        && [
            "use ",
            "please use ",
            "with ",
            "without ",
            "note ",
            "remember ",
            "使用",
            "改用",
            "注意",
            "补充",
            "记得",
            "不要",
            "只用",
        ]
        .iter()
        .any(|prefix| lower.starts_with(prefix));
    (explicit_continuation || bounded_constraint) && !has_new_boundary
}

fn multi_stage(prompt: &str) -> bool {
    has_stage_separator(prompt) || action_count(prompt) >= 2
}

fn has_stage_separator(prompt: &str) -> bool {
    prompt
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
        > 1
        || [
            " and ",
            " then ",
            ";",
            "；",
            "然后",
            "并且",
            "以及",
            "同时",
            "再进行",
        ]
        .iter()
        .any(|marker| prompt.contains(marker))
}

fn contains_engineering_action(prompt: &str) -> bool {
    action_count(prompt) > 0
}

fn action_count(prompt: &str) -> usize {
    let lower = prompt.to_lowercase();
    let words = english_words(&lower);
    let english = [
        "investigate",
        "research",
        "analyze",
        "implement",
        "fix",
        "update",
        "change",
        "add",
        "remove",
        "create",
        "build",
        "test",
        "verify",
        "run",
        "commit",
        "push",
        "install",
        "restart",
        "delegate",
    ]
    .iter()
    .filter(|signal| words.contains(*signal))
    .count();
    let chinese = [
        "调研", "分析", "实现", "修复", "修改", "更新", "增加", "删除", "创建", "构建", "测试",
        "验证", "执行", "提交", "推送", "安装", "重启", "委派",
    ]
    .iter()
    .filter(|signal| lower.contains(*signal))
    .count();
    english + chinese
}

fn english_words(prompt: &str) -> std::collections::BTreeSet<&str> {
    prompt
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .filter(|word| !word.is_empty())
        .collect()
}

fn plan_title(prompt: &str) -> String {
    const MAX_CHARS: usize = 80;
    let normalized = normalized(prompt);
    let mut title = normalized.chars().take(MAX_CHARS).collect::<String>();
    if normalized.chars().count() > MAX_CHARS {
        title.push('…');
    }
    if title.is_empty() {
        "Complete the requested work".to_owned()
    } else {
        title
    }
}

fn plan_steps(agent: &str) -> Vec<PlanningStepSeed> {
    let steps: &[(&str, &str)] = match agent {
        "plan" => &[
            ("investigate", "Establish the relevant facts"),
            ("decide", "Resolve the necessary decisions"),
            ("design", "Produce the implementation and acceptance plan"),
        ],
        "deep" => &[
            ("reproduce", "Reproduce and bound the failure"),
            ("diagnose", "Test hypotheses and identify the root cause"),
            ("implement", "Apply the root-cause fix"),
            ("verify", "Verify behavior and recovery"),
        ],
        "orchestrator" => &[
            ("investigate", "Establish scope and dependencies"),
            ("execute", "Execute or delegate non-overlapping work"),
            ("integrate", "Integrate results and reconcile state"),
            ("verify", "Run acceptance checks and report evidence"),
        ],
        _ => &[
            ("investigate", "Establish the relevant facts"),
            ("implement", "Implement the scoped change"),
            ("verify", "Run the relevant verification"),
        ],
    };
    steps
        .iter()
        .map(|(id, title)| PlanningStepSeed { id, title })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_research_modify_verify_work_creates_a_durable_plan() {
        let decision = PlanningPolicy::classify(PlanningInput::new(
            "Investigate the shell bug, implement the fix, and run the focused tests.",
            "build",
        ));

        assert!(matches!(decision, PlanningDecision::Create(_)));
    }

    #[test]
    fn cross_component_delegated_or_multi_gate_work_requires_a_plan() {
        for prompt in [
            "Update the CLI and engine, then run cargo test and clippy.",
            "Delegate the protocol audit and integrate the result.",
            "修复跨 crate 的调用链，并完成测试、clippy 和构建验收。",
        ] {
            assert!(
                matches!(
                    PlanningPolicy::classify(PlanningInput::new(prompt, "orchestrator")),
                    PlanningDecision::Create(_)
                ),
                "{prompt:?} must create a durable plan"
            );
        }
    }

    #[test]
    fn only_direct_answers_single_reads_and_true_atomic_actions_skip_creation() {
        for prompt in [
            "Why does this error happen?",
            "读取 Cargo.toml。",
            "Commit the current staged changes.",
        ] {
            assert!(
                matches!(
                    PlanningPolicy::classify(PlanningInput::new(prompt, "build")),
                    PlanningDecision::Atomic(_)
                ),
                "{prompt:?} should remain atomic"
            );
        }
    }

    #[test]
    fn an_explicit_continuation_maintains_the_existing_plan() {
        let decision = PlanningPolicy::classify(
            PlanningInput::new("Continue implementing and verify the change.", "build")
                .with_existing_plan(ExistingPlanState::Active),
        );

        assert!(matches!(decision, PlanningDecision::Maintain(_)));
        assert_eq!(decision.rationale().code(), "active_plan_continuation");
    }

    #[test]
    fn a_substantial_new_user_request_replaces_the_visible_active_plan() {
        let decision = PlanningPolicy::classify(
            PlanningInput::new(
                "另外，深入定位 GitHub Actions 失败并修复发布链路。",
                "orchestrator",
            )
            .with_existing_plan(ExistingPlanState::Active),
        );

        assert!(matches!(decision, PlanningDecision::Create(_)));
        assert_eq!(decision.rationale().code(), "active_plan_replaced");
    }

    #[test]
    fn a_short_constraint_followup_keeps_the_active_plan() {
        let decision = PlanningPolicy::classify(
            PlanningInput::new("使用 zsh 调用 gh，避免重复启动轮询。", "orchestrator")
                .with_existing_plan(ExistingPlanState::Active),
        );

        assert!(matches!(decision, PlanningDecision::Maintain(_)));
        assert_eq!(decision.rationale().code(), "active_plan_continuation");
    }

    #[test]
    fn a_changed_goal_objective_replaces_an_active_plan() {
        let decision = PlanningPolicy::classify(
            PlanningInput::new(
                "Audit the full release contract, fix the root cause, and verify it.",
                "orchestrator",
            )
            .with_source(PlanningInputSource::GoalObjective)
            .with_existing_plan(ExistingPlanState::Active),
        );

        assert!(matches!(decision, PlanningDecision::Create(_)));
        assert_eq!(decision.rationale().code(), "goal_objective_replaced");
    }

    #[test]
    fn a_goal_objective_that_starts_with_continue_still_creates_its_own_plan() {
        let decision = PlanningPolicy::classify(
            PlanningInput::new(
                "Continue implementing the release fix and verify the exact artifact.",
                "orchestrator",
            )
            .with_source(PlanningInputSource::GoalObjective)
            .with_existing_plan(ExistingPlanState::Active),
        );

        assert!(matches!(decision, PlanningDecision::Create(_)));
        assert_eq!(decision.rationale().code(), "goal_objective_replaced");
    }

    #[test]
    fn child_reports_never_create_a_plan_after_an_atomic_parent_request() {
        let decision = PlanningPolicy::classify(
            PlanningInput::new(
                "Implemented the fix and verified the focused tests.",
                "orchestrator",
            )
            .with_source(PlanningInputSource::ChildReport),
        );

        assert!(matches!(decision, PlanningDecision::Atomic(_)));
        assert_eq!(decision.rationale().code(), "non_user_input");
    }

    #[test]
    fn a_terminal_plan_allows_a_new_user_objective_to_create_a_plan() {
        let decision = PlanningPolicy::classify(
            PlanningInput::new(
                "Investigate the new failure, implement the fix, and verify it.",
                "build",
            )
            .with_existing_plan(ExistingPlanState::Terminal),
        );

        assert!(matches!(decision, PlanningDecision::Create(_)));
        assert_eq!(decision.rationale().code(), "terminal_plan_replaced");
    }

    #[test]
    fn structured_context_requires_a_plan_even_when_the_text_looks_like_a_question() {
        let decision = PlanningPolicy::classify(
            PlanningInput::new("Why does this fail?", "deep")
                .with_content(PlanningContentFacts::new(1, 1, 32_000, true)),
        );

        assert!(matches!(decision, PlanningDecision::Create(_)));
        assert_eq!(decision.rationale().code(), "typed_context");
    }

    #[test]
    fn multiline_work_is_not_collapsed_into_an_atomic_read() {
        let decision = PlanningPolicy::classify(PlanningInput::new(
            "Read Cargo.toml\nSummarize it and update the documentation.",
            "build",
        ));

        assert!(matches!(decision, PlanningDecision::Create(_)));
    }

    #[test]
    fn english_action_detection_uses_word_boundaries() {
        let decision =
            PlanningPolicy::classify(PlanningInput::new("What is address space?", "build"));

        assert!(matches!(decision, PlanningDecision::Atomic(_)));
    }

    #[test]
    fn todo_is_described_as_optional_detail_beneath_plan_steps() {
        let decision = PlanningPolicy::classify(PlanningInput::new(
            "Investigate, implement, and verify the runtime change.",
            "build",
        ));
        let seed = decision.seed().expect("multi-stage work creates a seed");

        assert_eq!(
            seed.steps()
                .iter()
                .map(|step| step.id())
                .collect::<Vec<_>>(),
            ["investigate", "implement", "verify"]
        );
        assert!(
            seed.guidance()
                .contains("Todo items are optional concrete work beneath plan steps")
        );
        assert!(!seed.guidance().contains("one Todo per Plan step"));
    }
}

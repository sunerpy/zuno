//! Host-owned durable planning policy.
//!
//! The host decides whether work requires a durable, user-visible Plan. It does
//! not invent generic steps: the model creates strategic steps through the
//! operation-based Plan tool, while machine execution phases stay in driver state.

/// Facts available before a turn reaches the provider.
#[derive(Debug, Clone, Copy)]
pub struct PlanningInput<'a> {
    prompt: &'a str,
    source: PlanningInputSource,
    existing_plan: ExistingPlanState,
    content: PlanningContentFacts,
    plan_available: bool,
}

impl<'a> PlanningInput<'a> {
    /// Build an input for a plan-capable Agent with no existing durable plan.
    #[must_use]
    pub const fn new(prompt: &'a str, _agent: &'a str) -> Self {
        Self {
            prompt,
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

/// Host planning outcome for one input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanningDecision {
    /// The model must create or replace a strategic Plan through the Plan tool.
    Required(PlanningRationale),
    /// Keep the existing plan current; never replace it with a generic seed.
    Maintain(PlanningRationale),
    /// A direct answer, single read, or genuinely atomic operation may proceed directly.
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
                || active_plan_continuation(prompt)
                || conversational(prompt))
        {
            return PlanningDecision::Maintain(reason(
                "active_plan_continuation",
                "a bounded answer, action, or explicit continuation keeps the active plan current",
            ));
        }
        if input.content.benefits_from_plan() {
            return require_plan(if input.source == PlanningInputSource::GoalObjective {
                "goal_objective_replaced"
            } else if input.existing_plan == ExistingPlanState::Active {
                "active_plan_replaced"
            } else {
                "typed_context"
            });
        }
        if direct_answer(prompt) {
            return PlanningDecision::Atomic(reason(
                "direct_answer",
                "a direct answer does not benefit from durable execution state",
            ));
        }
        if conversational(prompt) {
            return PlanningDecision::Atomic(reason(
                "conversational",
                "a greeting, thanks, or acknowledgement names no work and does not open a plan",
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
        if bounded_atomic_action(prompt) {
            return PlanningDecision::Atomic(reason(
                "bounded_atomic_action",
                "one explicitly bounded tool or operational action is atomic",
            ));
        }

        require_plan(
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

fn require_plan(code: &'static str) -> PlanningDecision {
    PlanningDecision::Required(reason(
        code,
        "the request benefits from progress visibility, dependency management, recovery, or verification",
    ))
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
        .any(|prefix| lower.starts_with(prefix))
        || asks_without_a_question_mark(prompt, &lower);
    question && !contains_engineering_action(&lower)
}

/// Longest prompt whose interior interrogative still makes it a plain question.
///
/// A question is short. The bound is what keeps a specification that happens to contain
/// `多少` — "make the retry cap configurable and record how many attempts were spent" —
/// from being read as one, and it matches [`bounded_atomic_action`]'s own ceiling.
const MEDIAL_QUESTION_MAX_CHARS: usize = 120;

/// Question forms that carry the interrogative anywhere but the front.
///
/// Prefixes and a trailing `?` are the wrong shape for Chinese, which routinely omits the
/// question mark and puts the marker where English puts it first: `你现在能看到多少个skill`
/// asks "how many skills can you see now" with `多少` in the middle and no `？` at all.
/// That prompt was classified as work requiring a durable Plan, and a question cannot be
/// answered by creating one — see [`crate::plan_driver::PlanReconciliationInput`] for what
/// the reconciliation loop then did with two further turns.
const MEDIAL_QUESTION_MARKERS: [&str; 16] = [
    "多少",
    "几个",
    "哪些",
    "哪个",
    "哪里",
    "是不是",
    "有没有",
    "能不能",
    "可不可以",
    "吗",
    "how many",
    "how much",
    "how long",
    "any idea",
    "is there",
    "are there",
];

/// Whether a short, single-stage prompt asks something without marking it as a question.
fn asks_without_a_question_mark(prompt: &str, lower: &str) -> bool {
    lower.chars().count() <= MEDIAL_QUESTION_MAX_CHARS
        && !multi_stage(prompt)
        && single_clause(prompt)
        && MEDIAL_QUESTION_MARKERS
            .iter()
            .any(|marker| lower.contains(marker))
}

/// Whether the prompt is one clause, which a prompt that is only a question will be.
///
/// A Chinese comma joins clauses, so `把重试上限做成可配置，并记录用了多少次尝试` mentions a
/// count inside an instruction rather than asking about one — and it names no action word
/// that [`action_count`] would catch. Requiring a single clause is what keeps an interior
/// interrogative from reaching work like that, and it costs a genuine question nothing: a
/// question asks one thing.
fn single_clause(prompt: &str) -> bool {
    !prompt.contains('，')
        && !prompt.contains(',')
        && !prompt.contains('、')
        && !["然后", "还要", "同时", "以及", " and ", " then "]
            .iter()
            .any(|joiner| prompt.contains(joiner))
}

/// Longest message that can be a greeting or acknowledgement rather than a request.
///
/// `thank you very much!` fits. A request that opens with a pleasantry runs longer, and
/// a longer message is judged on its verbs like any other prompt.
const CONVERSATIONAL_MAX_CHARS: usize = 24;

/// Words that may accompany a social marker without turning the message into work:
/// `thank you very much`, `hey there`, `thanks all`.
const CONVERSATIONAL_FILLER: [&str; 21] = [
    "there", "you", "u", "all", "so", "much", "very", "a", "lot", "and", "too", "again",
    "everyone", "team", "me", "guys", "friend", "mate", "good", "night", "nice",
];

/// Chinese particles that may accompany a marker: `谢谢你`, `好的呀`.
const CONVERSATIONAL_CJK_FILLER: [&str; 9] = ["你", "您", "了", "啦", "哦", "呀", "啊", "哈", "嘿"];

/// Words that mark a social message. ASCII entries match whole words; CJK entries match
/// as substrings, as [`action_count`] already does for Chinese.
const CONVERSATIONAL_MARKERS: [&str; 30] = [
    "hi",
    "hello",
    "hey",
    "hiya",
    "howdy",
    "thanks",
    "thank",
    "thx",
    "cheers",
    "morning",
    "afternoon",
    "evening",
    "bye",
    "goodbye",
    "ok",
    "okay",
    "你好",
    "您好",
    "嗨",
    "哈喽",
    "早上好",
    "早安",
    "下午好",
    "晚上好",
    "晚安",
    "谢谢",
    "多谢",
    "感谢",
    "好的",
    "再见",
];

/// Whether the prompt is a greeting, thanks, or bare acknowledgement.
///
/// `hi` reached the `require_plan` fallthrough because it is neither a question nor a
/// read, commit, or bounded action. The runtime instruction then told the `deep` Agent to
/// read and create a durable Plan, and it opened one titled "Acknowledge greeting" before
/// saying hello. A message that names no work is not an objective: it never opens a Plan
/// and, when one is active, it continues that Plan rather than replacing it. Short and
/// action-free on purpose, so `thanks, now fix the failing test` still classifies as work.
fn conversational(prompt: &str) -> bool {
    let lower = normalized(prompt).to_lowercase();
    if lower.chars().count() > CONVERSATIONAL_MAX_CHARS
        || multi_stage(prompt)
        || action_count(prompt) > 0
    {
        return false;
    }
    // The message must be *bare*: once every social marker and filler word is taken
    // out, nothing else may remain. `hi, review my PR` and `ok, deploy prod` carry a
    // marker but also name work in verbs the action list does not know, and a request
    // is never demoted to small talk because it opened politely.
    let mut saw_marker = false;
    let mut rest = lower.clone();
    for marker in CONVERSATIONAL_MARKERS
        .iter()
        .filter(|marker| !marker.is_ascii())
    {
        if rest.contains(marker) {
            saw_marker = true;
            rest = rest.replace(marker, " ");
        }
    }
    for particle in CONVERSATIONAL_CJK_FILLER {
        rest = rest.replace(particle, " ");
    }
    if rest
        .chars()
        .any(|character| character.is_alphabetic() && !character.is_ascii())
    {
        return false;
    }
    for word in english_words(&rest) {
        if CONVERSATIONAL_MARKERS.contains(&word) {
            saw_marker = true;
        } else if !CONVERSATIONAL_FILLER.contains(&word) {
            return false;
        }
    }
    saw_marker
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

fn bounded_atomic_action(prompt: &str) -> bool {
    let lower = normalized(prompt).to_lowercase();
    let actions = action_count(prompt);
    let tool_invocation = ["use ", "please use ", "call ", "invoke ", "使用", "调用"]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
        && actions == 0;
    let one_step_operation = [
        "install ", "restart ", "run ", "execute ", "安装", "重启", "执行",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
        && actions <= 2;
    (tool_invocation || one_step_operation)
        && !has_stage_separator(prompt)
        && lower.chars().count() <= 120
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_research_modify_verify_work_creates_a_durable_plan() {
        let decision = PlanningPolicy::classify(PlanningInput::new(
            "Investigate the shell bug, implement the fix, and run the focused tests.",
            "build",
        ));

        assert!(matches!(decision, PlanningDecision::Required(_)));
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
                    PlanningDecision::Required(_)
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
            "Use get_weather for Paris.",
            "Install the spreadsheet skill.",
            "执行 cargo test -p zuno-network。",
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
    fn a_tool_hint_does_not_hide_substantial_engineering_work() {
        let decision = PlanningPolicy::classify(PlanningInput::new(
            "Use Rust to implement the authentication feature.",
            "build",
        ));

        assert!(matches!(decision, PlanningDecision::Required(_)));
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

        assert!(matches!(decision, PlanningDecision::Required(_)));
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

        assert!(matches!(decision, PlanningDecision::Required(_)));
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

        assert!(matches!(decision, PlanningDecision::Required(_)));
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

        assert!(matches!(decision, PlanningDecision::Required(_)));
        assert_eq!(decision.rationale().code(), "terminal_plan_replaced");
    }

    #[test]
    fn structured_context_requires_a_plan_even_when_the_text_looks_like_a_question() {
        let decision = PlanningPolicy::classify(
            PlanningInput::new("Why does this fail?", "deep")
                .with_content(PlanningContentFacts::new(1, 1, 32_000, true)),
        );

        assert!(matches!(decision, PlanningDecision::Required(_)));
        assert_eq!(decision.rationale().code(), "typed_context");
    }

    #[test]
    fn multiline_work_is_not_collapsed_into_an_atomic_read() {
        let decision = PlanningPolicy::classify(PlanningInput::new(
            "Read Cargo.toml\nSummarize it and update the documentation.",
            "build",
        ));

        assert!(matches!(decision, PlanningDecision::Required(_)));
    }

    #[test]
    fn a_question_without_a_question_mark_is_still_a_question() {
        // The reported session. The user asked how many skills were visible, the model
        // answered, and because this was classified `Required` while the model correctly
        // created no Plan, reconciliation spent two more turns asking for progress — the
        // second of which offered to enumerate the whole catalog page by page.
        for prompt in [
            "你现在能看到多少个skill",
            "现在有几个 skill 可用",
            "这个配置项是不是必须的",
            "会话里还有多少 token",
            "how many skills are loaded right now",
        ] {
            let decision = PlanningPolicy::classify(PlanningInput::new(prompt, "orchestrator"));
            assert!(
                matches!(decision, PlanningDecision::Atomic(_)),
                "{prompt:?} is a question and must not require a durable Plan: {decision:?}"
            );
            assert_eq!(decision.rationale().code(), "direct_answer");
        }
    }

    #[test]
    fn an_interior_question_word_does_not_make_real_work_atomic() {
        // The bound that keeps the marker from swallowing requests that only mention a
        // count, name a stage, or ask for work in the same breath as a question.
        for prompt in [
            "把重试上限做成可配置，并记录用了多少次尝试",
            "看看有多少个 skill 缺失，然后补齐并验证",
            "统计一下有多少个 crate 依赖 zuno-engine，实现缓存后再测试一遍这条路径的耗时表现",
        ] {
            assert!(
                matches!(
                    PlanningPolicy::classify(PlanningInput::new(prompt, "orchestrator")),
                    PlanningDecision::Required(_)
                ),
                "{prompt:?} asks for work and must still create a durable Plan"
            );
        }
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

        assert!(
            decision
                .guidance()
                .contains("Todo items are optional concrete work beneath plan steps")
        );
        assert!(!decision.guidance().contains("one Todo per Plan step"));
    }

    #[test]
    fn a_greeting_or_acknowledgement_never_requires_a_plan() {
        // The reported session. `hi` to the `deep` Agent matched no atomic shape and reached
        // the `require_plan` fallthrough; the runtime instruction then told the model to read
        // and create a durable Plan, and it opened one titled "Acknowledge greeting" before
        // saying hello. A message that names no work is not an objective.
        for prompt in [
            "hi",
            "Hi!",
            "hello",
            "hey there",
            "thanks",
            "thank you",
            "ok",
            "good morning",
            "你好",
            "谢谢",
            "好的",
            "thank you very much!",
            "thanks all",
            "谢谢你",
            "好的呀",
        ] {
            let decision = PlanningPolicy::classify(PlanningInput::new(prompt, "deep"));
            assert!(
                matches!(decision, PlanningDecision::Atomic(_)),
                "{prompt:?} names no work and must not require a durable Plan: {decision:?}"
            );
            assert_eq!(decision.rationale().code(), "conversational", "{prompt:?}");
        }
    }

    #[test]
    fn a_greeting_with_an_active_plan_maintains_it() {
        for prompt in ["thanks", "hi", "好的"] {
            let decision = PlanningPolicy::classify(
                PlanningInput::new(prompt, "build").with_existing_plan(ExistingPlanState::Active),
            );
            assert!(
                matches!(decision, PlanningDecision::Maintain(_)),
                "{prompt:?} must keep the active plan, not replace it: {decision:?}"
            );
            assert_eq!(decision.rationale().code(), "active_plan_continuation");
        }
    }

    #[test]
    fn a_greeting_after_a_finished_plan_does_not_replace_it() {
        let decision = PlanningPolicy::classify(
            PlanningInput::new("thanks", "build").with_existing_plan(ExistingPlanState::Terminal),
        );

        assert!(
            matches!(decision, PlanningDecision::Atomic(_)),
            "{decision:?}"
        );
        assert_eq!(decision.rationale().code(), "conversational");
    }

    #[test]
    fn a_greeting_with_typed_context_still_selects_the_planned_path() {
        // Same rule as a question: an image, resource, selection, or branch diff attached
        // to the text is the work, whatever the text says.
        let decision = PlanningPolicy::classify(
            PlanningInput::new("thanks", "deep")
                .with_content(PlanningContentFacts::new(1, 1, 32_000, true)),
        );

        assert!(
            matches!(decision, PlanningDecision::Required(_)),
            "{decision:?}"
        );
        assert_eq!(decision.rationale().code(), "typed_context");
    }

    #[test]
    fn a_social_word_does_not_hide_work() {
        for prompt in [
            "thanks, now fix the failing test and rerun it",
            "hello, implement the auth feature",
            "好的，修复这个 bug 并测试",
            // Short, polite, and naming work in a verb the action list does not know:
            // a marker beside any other word is a request, not small talk.
            "hi, review my PR",
            "ok, deploy prod",
            "thanks, merge the PR",
            "great, deploy prod",
            "好的，部署一下",
            "hello world in rust",
        ] {
            let decision = PlanningPolicy::classify(PlanningInput::new(prompt, "deep"));
            assert!(
                matches!(decision, PlanningDecision::Required(_)),
                "{prompt:?} asks for work and must still create a durable Plan: {decision:?}"
            );
        }
    }
}

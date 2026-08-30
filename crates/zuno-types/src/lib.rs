//! Wire and domain types shared across the workspace (sessions, messages, parts, tool payloads).

/// One class of routine work a client may compact in its main timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActivityKind {
    Command,
    Read,
    Search,
    Delegation,
    Image,
    Tool,
}

/// Frontend-neutral counts for one model step's routine activity.
///
/// Clients decide how to render these counts. The projection deliberately carries no
/// terminal glyphs, key names, or localized copy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ActivityProjection {
    pub commands: usize,
    pub reads: usize,
    pub searches: usize,
    pub delegations: usize,
    pub images: usize,
    pub tools: usize,
}

impl ActivityProjection {
    pub fn record(&mut self, kind: ActivityKind) {
        let slot = match kind {
            ActivityKind::Command => &mut self.commands,
            ActivityKind::Read => &mut self.reads,
            ActivityKind::Search => &mut self.searches,
            ActivityKind::Delegation => &mut self.delegations,
            ActivityKind::Image => &mut self.images,
            ActivityKind::Tool => &mut self.tools,
        };
        *slot = slot.saturating_add(1);
    }

    #[must_use]
    pub const fn total(self) -> usize {
        self.commands
            .saturating_add(self.reads)
            .saturating_add(self.searches)
            .saturating_add(self.delegations)
            .saturating_add(self.images)
            .saturating_add(self.tools)
    }
}

/// Disjoint token buckets shared by durable storage and every client surface.
///
/// `unclassified` is used by work-state meters that receive only a trustworthy
/// aggregate from a child process. It keeps that total honest without pretending the
/// provider identified it as prompt, completion, reasoning, or cache usage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub unclassified: u64,
}

impl TokenUsage {
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.total() == 0
    }

    #[must_use]
    pub const fn total(self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.reasoning)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
            .saturating_add(self.unclassified)
    }

    #[must_use]
    pub const fn unclassified(total: u64) -> Self {
        Self {
            input: 0,
            output: 0,
            reasoning: 0,
            cache_read: 0,
            cache_write: 0,
            unclassified: total,
        }
    }

    /// Add a provider report that has no separate reasoning bucket.
    pub const fn add(&mut self, input: u64, output: u64, cache_read: u64, cache_write: u64) {
        self.input = self.input.saturating_add(input);
        self.output = self.output.saturating_add(output);
        self.cache_read = self.cache_read.saturating_add(cache_read);
        self.cache_write = self.cache_write.saturating_add(cache_write);
    }

    pub const fn add_usage(&mut self, usage: Self) {
        self.input = self.input.saturating_add(usage.input);
        self.output = self.output.saturating_add(usage.output);
        self.reasoning = self.reasoning.saturating_add(usage.reasoning);
        self.cache_read = self.cache_read.saturating_add(usage.cache_read);
        self.cache_write = self.cache_write.saturating_add(usage.cache_write);
        self.unclassified = self.unclassified.saturating_add(usage.unclassified);
    }
}

/// How a provider's prompt count relates to its cache buckets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UsageAccounting {
    #[default]
    Unknown,
    CacheInsideInput,
    CacheBesideInput,
}

impl UsageAccounting {
    #[must_use]
    pub const fn as_str(self) -> Option<&'static str> {
        match self {
            Self::Unknown => None,
            Self::CacheInsideInput => Some("cache-inside-input"),
            Self::CacheBesideInput => Some("cache-beside-input"),
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "cache-inside-input" => Self::CacheInsideInput,
            "cache-beside-input" => Self::CacheBesideInput,
            _ => Self::Unknown,
        }
    }

    #[must_use]
    pub const fn prompt_total(self, usage: TokenUsage) -> Option<u64> {
        match self {
            Self::Unknown => None,
            Self::CacheInsideInput => Some(usage.input),
            Self::CacheBesideInput => Some(
                usage
                    .input
                    .saturating_add(usage.cache_read)
                    .saturating_add(usage.cache_write),
            ),
        }
    }

    #[must_use]
    pub const fn normalize(self, usage: TokenUsage) -> Option<TokenUsage> {
        match self {
            Self::Unknown => None,
            Self::CacheInsideInput => Some(TokenUsage {
                input: usage
                    .input
                    .saturating_sub(usage.cache_read)
                    .saturating_sub(usage.cache_write),
                ..usage
            }),
            Self::CacheBesideInput => Some(usage),
        }
    }
}

/// Durable usage state for a session or execution owner.
///
/// Confirmed counters never decrease on a failed request. A request rejected before a
/// provider usage frame keeps its local estimate separately, so clients render `≈N`
/// instead of replacing the last trustworthy value with zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageSnapshot {
    pub confirmed: TokenUsage,
    pub last_prompt_tokens: Option<u64>,
    pub estimated_pending_prompt_tokens: Option<u64>,
    pub context_limit: Option<u64>,
    pub accounting: UsageAccounting,
    pub confirmed_known: bool,
    pub last_confirmed_at: Option<i64>,
    pub failed_turns: u64,
    pub last_failed_at: Option<i64>,
}

/// Timing and usage owned by one Goal, Plan, WorkItem, Job, or workflow node.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExecutionSpan {
    pub started_at: i64,
    pub completed_at: Option<i64>,
    pub elapsed_ms: u64,
    pub usage: TokenUsage,
    pub accounting_known: bool,
}

impl ExecutionSpan {
    #[must_use]
    pub const fn from_aggregate(
        started_at: i64,
        completed_at: Option<i64>,
        elapsed_ms: u64,
        tokens: u64,
        accounting_known: bool,
    ) -> Self {
        Self {
            started_at,
            completed_at,
            elapsed_ms,
            usage: TokenUsage::unclassified(tokens),
            accounting_known,
        }
    }
}

/// Resident-memory scope shared by storage, tools, and clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemoryScope {
    Global,
    Project,
}

impl MemoryScope {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "global" => Some(Self::Global),
            "project" => Some(Self::Project),
            _ => None,
        }
    }
}

/// One proposed resident-memory mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemoryAction {
    Add,
    Replace,
    Remove,
}

impl MemoryAction {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Replace => "replace",
            Self::Remove => "remove",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "add" => Some(Self::Add),
            "replace" => Some(Self::Replace),
            "remove" => Some(Self::Remove),
            _ => None,
        }
    }
}

/// Provenance for a durable memory candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemorySource {
    Reflection,
    Tool,
    User,
}

impl MemorySource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reflection => "reflection",
            Self::Tool => "tool",
            Self::User => "user",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "reflection" => Some(Self::Reflection),
            "tool" => Some(Self::Tool),
            "user" => Some(Self::User),
            _ => None,
        }
    }
}

/// Durable lifecycle of one proposed memory change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemoryCandidateStatus {
    Pending,
    Applying,
    Undoing,
    Applied,
    Rejected,
    Undone,
    Failed,
    Uncertain,
}

impl MemoryCandidateStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Applying => "applying",
            Self::Undoing => "undoing",
            Self::Applied => "applied",
            Self::Rejected => "rejected",
            Self::Undone => "undone",
            Self::Failed => "failed",
            Self::Uncertain => "uncertain",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "applying" => Some(Self::Applying),
            "undoing" => Some(Self::Undoing),
            "applied" => Some(Self::Applied),
            "rejected" => Some(Self::Rejected),
            "undone" => Some(Self::Undone),
            "failed" => Some(Self::Failed),
            "uncertain" => Some(Self::Uncertain),
            _ => None,
        }
    }
}

/// Client-neutral memory candidate with audit provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryCandidateProjection {
    pub id: String,
    pub scope: MemoryScope,
    pub action: MemoryAction,
    pub content: Option<String>,
    pub old_text: Option<String>,
    pub reason: String,
    /// Confidence in basis points (`0..=10_000`).
    pub confidence: u16,
    pub source: MemorySource,
    pub source_session_id: Option<String>,
    pub source_message_id: Option<String>,
    pub status: MemoryCandidateStatus,
    pub error: Option<String>,
    pub time_created: i64,
    pub time_updated: i64,
}

/// One current resident-memory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntryProjection {
    pub scope: MemoryScope,
    pub content: String,
}

/// Active goal summary shown by clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalPauseProjection {
    pub reason: String,
    pub human_request_id: Option<String>,
    pub time_paused: i64,
}

/// One persisted cross-turn Goal retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalRetryProjection {
    pub attempt: u32,
    pub reason: String,
    pub delay_ms: i64,
    pub retry_at_ms: i64,
    pub scheduled_at_ms: i64,
}

/// One persisted provider-request backoff checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderBackoffProjection {
    pub request_id: String,
    pub turn_id: String,
    pub failed_attempt: u32,
    pub next_attempt: u32,
    pub max_attempts: u32,
    pub reason: String,
    pub delay_ms: i64,
    pub retry_at_ms: i64,
    pub scheduled_at_ms: i64,
}

/// One durable human request currently blocking Goal continuation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HumanRequestProjection {
    pub id: String,
    pub kind: String,
    pub summary: Option<String>,
    pub time_created: i64,
}

/// Active goal summary shown by clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalStateProjection {
    pub id: String,
    pub revision: i64,
    pub objective: String,
    pub success_criteria: Vec<String>,
    pub status: String,
    pub blocked_reason: Option<String>,
    pub span: ExecutionSpan,
    pub token_budget: Option<i64>,
    pub pause: Option<GoalPauseProjection>,
    pub retry: Option<GoalRetryProjection>,
    pub provider_backoff: Option<ProviderBackoffProjection>,
    pub pending_human_requests: Vec<HumanRequestProjection>,
    pub time_created: i64,
    pub time_updated: i64,
}

/// One stable step in the current durable plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStepProjection {
    pub id: String,
    pub title: String,
    pub status: String,
}

/// The current durable plan shown by clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanProjection {
    pub id: String,
    pub goal_id: Option<String>,
    pub revision: i64,
    pub title: String,
    pub steps: Vec<PlanStepProjection>,
    pub span: ExecutionSpan,
    pub time_created: i64,
    pub time_updated: i64,
}

/// One durable work item shown by clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoProjection {
    pub id: String,
    pub goal_id: Option<String>,
    pub plan_step_id: Option<String>,
    pub parent_id: Option<String>,
    pub subject: String,
    pub description: String,
    pub active_form: Option<String>,
    pub status: String,
    pub priority: String,
    pub dependencies: Vec<String>,
    pub owner: Option<String>,
    pub revision: i64,
    pub span: ExecutionSpan,
    pub time_created: i64,
    pub time_updated: i64,
}

/// The typed subject owned by one durable background job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobSubjectProjection {
    ChildSession {
        session_id: String,
    },
    ProductAgent {
        run_id: String,
        product: String,
        instance: String,
        tool: String,
    },
    Workflow {
        run_id: String,
        workflow: String,
    },
    Council {
        run_id: String,
        preset: String,
    },
}

/// One durable child work item owned by a workflow or Council job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobChildProjection {
    pub id: String,
    pub subject: String,
    pub owner: Option<String>,
    pub status: String,
    pub span: ExecutionSpan,
}

/// One durable background job shown by clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobProjection {
    pub id: String,
    pub subject: JobSubjectProjection,
    pub status: String,
    pub report_delivery: String,
    pub result: Option<String>,
    pub error: Option<String>,
    pub span: ExecutionSpan,
    pub children: Vec<JobChildProjection>,
    pub time_created: i64,
    pub time_completed: Option<i64>,
}

/// One durable background terminal shown by clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundExecutionProjection {
    pub id: String,
    pub title: String,
    pub command: String,
    pub status: String,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub error: Option<String>,
    pub span: ExecutionSpan,
    pub time_created: i64,
    pub time_completed: Option<i64>,
}

/// Frontend-neutral durable work state for one session and project.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkStateProjection {
    pub goal: Option<GoalStateProjection>,
    pub plan: Option<PlanProjection>,
    pub todos: Vec<TodoProjection>,
    pub background_executions: Vec<BackgroundExecutionProjection>,
    pub jobs: Vec<JobProjection>,
    pub memory_candidates: Vec<MemoryCandidateProjection>,
    pub memory_entries: Vec<MemoryEntryProjection>,
}

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
pub struct GoalStateProjection {
    pub objective: String,
    pub status: String,
    pub tokens_used: i64,
    pub token_budget: Option<i64>,
}

/// One durable todo row shown by clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoProjection {
    pub content: String,
    pub status: String,
    pub priority: String,
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
    pub time_created: i64,
    pub time_completed: Option<i64>,
}

/// Frontend-neutral durable work state for one session and project.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkStateProjection {
    pub goal: Option<GoalStateProjection>,
    pub todos: Vec<TodoProjection>,
    pub jobs: Vec<JobProjection>,
    pub memory_candidates: Vec<MemoryCandidateProjection>,
    pub memory_entries: Vec<MemoryEntryProjection>,
}

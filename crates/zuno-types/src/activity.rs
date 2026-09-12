//! Public client activity. Provider replay blocks, credentials, leases and
//! configuration snapshots have no representation in this protocol.

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::borrow::Cow;

use crate::identity::{
    ApprovalId, EnvironmentId, InvocationId, JobId, OperationId, SessionId, TurnId,
};

pub const ACTIVITY_PROTOCOL_VERSION: u32 = 1;
pub const MAX_ACTIVITY_PAGE_BYTES: usize = 1024 * 1024;
pub const MAX_ACTIVITY_TEXT_BYTES: usize = 128 * 1024;

/// Exact logical counters and timestamps are decimal strings on the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Counter(pub u64);

impl TryFrom<String> for Counter {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        let parsed: u64 = value.parse().map_err(|_| "invalid decimal counter")?;
        if parsed.to_string() != value {
            return Err("counter is not canonical decimal");
        }
        Ok(Self(parsed))
    }
}
impl From<Counter> for String {
    fn from(value: Counter) -> Self {
        value.0.to_string()
    }
}
impl JsonSchema for Counter {
    fn schema_name() -> Cow<'static, str> {
        "Counter".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","pattern":"^(0|[1-9][0-9]{0,19})$","maxLength":20})
    }
}

/// Validated display/wire name for dynamic tools, Agents and providers. It is
/// data, never a selector for a built-in capability or an authorization grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ActivityName(String);
impl ActivityName {
    pub fn new(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 256
            || value.trim() != value
            || value.chars().any(char::is_control)
        {
            return Err("invalid activity name");
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for ActivityName {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl From<ActivityName> for String {
    fn from(value: ActivityName) -> Self {
        value.0
    }
}
impl JsonSchema for ActivityName {
    fn schema_name() -> Cow<'static, str> {
        "ActivityName".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","minLength":1,"maxLength":256})
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InvocationAction {
    FileRead,
    FileList,
    FileSearch,
    FileEdit,
    Process,
    WebSearch,
    WebFetch,
    MemoryRead,
    MemoryWrite,
    Agent,
    Workflow,
    Council,
    #[default]
    Tool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum InvocationSource {
    Builtin,
    Mcp {
        server: ActivityName,
        tool: ActivityName,
        exposure: McpExposure,
    },
    Extension {
        extension: ActivityName,
    },
    Provider {
        provider: ActivityName,
    },
    ExternalAgent {
        agent: ActivityName,
    },
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpExposure {
    Exposed,
    Deferred,
    Unavailable,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InvocationPresentation {
    pub action: InvocationAction,
    pub source: InvocationSource,
}

impl InvocationPresentation {
    pub const fn builtin(action: InvocationAction) -> Self {
        Self {
            action,
            source: InvocationSource::Builtin,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InvocationState {
    Queued,
    Waiting,
    Running,
    Succeeded,
    Failed,
    Denied,
    Cancelled,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum WaitingFor {
    Approval { approval_id: ApprovalId },
    UserInput { question_id: String },
    Child { job_id: JobId },
    Operation { operation_id: OperationId },
    Timer { deadline: Counter },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ExecutionLocation {
    Local,
    Enterprise {
        environment_id: EnvironmentId,
    },
    External {
        name: ActivityName,
    },
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    Enforced,
    Unconfined,
    Unavailable,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DenialReason {
    Policy,
    Permission,
    Expired,
    Unavailable,
    Cancelled,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceRef {
    pub id: String,
    pub name: String,
    pub media_type: Option<String>,
    pub bytes: Option<Counter>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ContentBlock {
    Text {
        text: String,
        truncated: bool,
    },
    Code {
        code: String,
        language: Option<String>,
        truncated: bool,
    },
    Diff {
        diff: String,
        truncated: bool,
    },
    Terminal {
        text: String,
        channel: TerminalChannel,
        truncated: bool,
    },
    Structured {
        value: Value,
    },
    Image {
        resource: ResourceRef,
        alt: String,
    },
    Resource {
        resource: ResourceRef,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TerminalChannel {
    Stdout,
    Stderr,
    Combined,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum UiAction {
    View { resource_id: String },
    Approve { approval_id: ApprovalId },
    Reject { approval_id: ApprovalId },
    Answer { question_id: String },
    Interrupt { job_id: JobId, turn_id: TurnId },
    RequestResume { job_id: JobId },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Invocation {
    pub id: InvocationId,
    pub name: ActivityName,
    pub presentation: InvocationPresentation,
    pub state: InvocationState,
    pub input: Value,
    pub content: Vec<ContentBlock>,
    pub waiting_for: Option<WaitingFor>,
    pub location: ExecutionLocation,
    pub isolation: Isolation,
    pub denial: Option<DenialReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MessageOrigin {
    UserInput,
    Model,
    AgentReport,
    Runtime,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MessageState {
    Pending,
    Complete,
    Interrupted,
    Failed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Rejected,
    Expired,
    Revoked,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkState {
    Pending,
    Active,
    Waiting,
    Paused,
    Completed,
    Failed,
    Cancelled,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlanStep {
    pub id: String,
    pub text: String,
    pub status: crate::PlanStepStatus,
}

/// Input counts exclude cache reads/writes; reasoning is included in output.
/// Unknown accounting is omitted instead of reporting fabricated zeros.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NormalizedUsage {
    pub input: Counter,
    pub output: Counter,
    pub reasoning: Counter,
    pub cache_read: Counter,
    pub cache_write: Counter,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum SessionItem {
    Message {
        role: MessageRole,
        origin: MessageOrigin,
        state: MessageState,
        content: Vec<ContentBlock>,
        usage: Option<NormalizedUsage>,
    },
    Thinking {
        text: String,
        collapsed: bool,
        truncated: bool,
    },
    Invocation {
        invocation: Box<Invocation>,
    },
    Approval {
        approval_id: ApprovalId,
        job_id: JobId,
        status: ApprovalStatus,
        presentation: Vec<ContentBlock>,
    },
    Plan {
        steps: Vec<PlanStep>,
    },
    Goal {
        objective: String,
        state: WorkState,
    },
    Compaction {
        automatic: bool,
    },
    Artifact {
        resource: ResourceRef,
    },
    Background {
        job_id: JobId,
        label: String,
        state: WorkState,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ItemRecord {
    pub id: String,
    pub parent_id: Option<String>,
    pub created_at: Counter,
    pub item: SessionItem,
    pub actions: Vec<UiAction>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommittedEvent {
    Upsert {
        position: Counter,
        record: Box<ItemRecord>,
    },
    Remove {
        id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommittedFrame {
    pub version: u32,
    pub session_id: SessionId,
    pub sequence: Counter,
    pub event: CommittedEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum LiveEvent {
    Snapshot {
        items: Vec<LiveItem>,
    },
    TextDelta {
        item_id: String,
        text: String,
    },
    ThinkingDelta {
        item_id: String,
        text: String,
    },
    InvocationProgress {
        invocation_id: InvocationId,
        label: String,
    },
    Reset,
}

/// Replaceable drafts have independent IDs and are discarded when their
/// generation ends. Only committed records enter durable conversation history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum LiveItem {
    Text {
        id: String,
        parent_id: Option<String>,
        text: String,
        truncated: bool,
    },
    Thinking {
        id: String,
        parent_id: Option<String>,
        text: String,
        truncated: bool,
    },
    Invocation {
        id: InvocationId,
        label: String,
    },
}

/// A replaceable, bounded stream generation. It never substitutes for committed
/// history and cannot carry encrypted reasoning or authoritative usage totals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LiveFrame {
    pub version: u32,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub generation: String,
    pub sequence: Counter,
    pub after_committed: Counter,
    pub event: LiveEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HistoryItem {
    pub position: Counter,
    pub revision: Counter,
    pub record: ItemRecord,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HistoryPage {
    pub version: u32,
    pub session_id: SessionId,
    pub through: Counter,
    pub items: Vec<HistoryItem>,
    pub before: Option<Counter>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FramePage {
    pub version: u32,
    pub session_id: SessionId,
    pub frames: Vec<CommittedFrame>,
    pub through: Counter,
    pub more: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn counters_are_exact_and_names_cannot_smuggle_control_text() {
        let max = serde_json::to_value(Counter(u64::MAX)).unwrap();
        assert_eq!(max, Value::String(u64::MAX.to_string()));
        assert_eq!(
            serde_json::from_value::<Counter>(max).unwrap(),
            Counter(u64::MAX)
        );
        for invalid in ["00", "-1", "1.0", " 2", "18446744073709551616"] {
            assert!(serde_json::from_value::<Counter>(Value::String(invalid.to_owned())).is_err());
        }
        for invalid in ["", " tool", "mcp\nadmin", "\u{001b}secret"] {
            assert!(ActivityName::new(invalid).is_err());
        }
    }
}

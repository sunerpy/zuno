//! Bounded, source-linked task understanding. This is not permission or wake authority.

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;
use zuno_db::Pool;
use zuno_db::event_log::{NewSessionEvent, append_in, latest_of_type_in};
use zuno_error::{DbError, ToolError};
use zuno_orchestration::sha256_json;
use zuno_tool::{
    HistoryPolicy, PermissionAsk, ToolContext, ToolDynamicContextRefresh, ToolEffect, ToolOutput,
    ToolReplayPolicy, TypedTool,
};

pub const ID: &str = "task_context";
pub const DESCRIPTION: &str = include_str!("description/task-context.txt");
const EVENT: &str = "session.task_context.updated";
const MAX_BYTES: usize = 24 * 1024;

// One provenance predicate for explicit sources, recent listings and prompt
// freshness. Filter runtime-generated user-shaped rows before applying LIMIT.
const USER_SOURCE_IDS: &str = "
    SELECT m.id FROM message m
    WHERE m.session_id=?1 AND (?2 IS NULL OR m.id=?2)
      AND json_extract(m.data,'$.role')='user'
      AND json_type(m.data,'$.taskReport') IS NULL
      AND COALESCE(json_extract(m.data,'$.synthetic'),0)=0
      AND COALESCE(json_extract(m.data,'$.mode'),'')<>'compaction'
      AND NOT EXISTS (
        SELECT 1 FROM part p WHERE p.session_id=m.session_id AND p.message_id=m.id
          AND json_extract(p.data,'$.type')='compaction')
      AND NOT EXISTS (
        SELECT 1 FROM session_input i WHERE i.session_id=m.session_id
          AND (i.id=m.id OR json_extract(i.prompt,'$.message.id')=m.id)
          AND i.trigger_kind NOT IN ('user','user_control','legacy'))
    ORDER BY m.time_created DESC,m.id DESC LIMIT ?3";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskIntent {
    Delivery,
    Inspection,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    #[default]
    Active,
    Waiting,
    Complete,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DecisionOwner {
    #[default]
    Agent,
    User,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    Delivery,
    Safety,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    #[default]
    Pending,
    Passed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskCheck {
    pub id: String,
    pub kind: CheckKind,
    pub description: String,
    #[serde(default)]
    pub status: CheckStatus,
    /// Checkable references, not assertions that the host has certified their content.
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BlockerKind {
    UserAuthority,
    UserInformation,
    ExternalDependency,
    SafetyGate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskBlocker {
    pub kind: BlockerKind,
    pub reason: String,
    #[serde(default)]
    pub reference: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UserSource {
    pub message_id: String,
    pub digest: String,
    pub time_created: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskContextSnapshot {
    pub task_id: String,
    pub revision: i64,
    pub objective: String,
    pub intent: TaskIntent,
    pub authorized_actions: Vec<String>,
    pub prohibitions: Vec<String>,
    pub sources: Vec<UserSource>,
    pub checks: Vec<TaskCheck>,
    pub status: TaskStatus,
    pub decision_owner: DecisionOwner,
    pub blocker: Option<TaskBlocker>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    #[default]
    Get,
    Update,
    Replace,
}

/// Update is a patch: omitted fields retain previous intent; prohibitions and
/// checks are merged, not silently dropped. Replace explicitly starts a new task.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Params {
    pub action: Action,
    #[serde(default)]
    pub expected_revision: Option<i64>,
    #[serde(default)]
    pub objective: Option<String>,
    #[serde(default)]
    pub task_kind: Option<TaskIntent>,
    #[serde(default)]
    pub authorized_actions: Option<Vec<String>>,
    #[serde(default)]
    pub prohibitions: Vec<String>,
    #[serde(default)]
    pub source_message_ids: Vec<String>,
    #[serde(default)]
    pub checks: Vec<TaskCheck>,
    /// Explicitly revise check descriptions with user sources. Revised checks
    /// must be pending and have no evidence from their old definition.
    #[serde(default)]
    pub revise_checks: bool,
    #[serde(default)]
    pub status: Option<TaskStatus>,
    #[serde(default)]
    pub decision_owner: Option<DecisionOwner>,
    #[serde(default)]
    pub blocker: Option<TaskBlocker>,
}

#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error(transparent)]
    Database(#[from] DbError),
    #[error("task context revision changed; read task_context before retrying")]
    Conflict,
    #[error("invalid task context: {0}")]
    Invalid(&'static str),
}

#[derive(Clone)]
pub struct TaskContextTool {
    pool: Arc<Pool>,
}

impl TaskContextTool {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }
}

fn decode(value: Value) -> Result<TaskContextSnapshot, ContextError> {
    serde_json::from_value(value).map_err(|_| ContextError::Invalid("corrupt stored context"))
}

pub fn read_in(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<TaskContextSnapshot>, ContextError> {
    let Some(event) = latest_of_type_in(connection, session_id, EVENT)? else {
        return Ok(None);
    };
    if event.version != 1 {
        return Err(ContextError::Invalid(
            "unsupported task context event version",
        ));
    }
    let value = event
        .properties
        .get("context")
        .ok_or(ContextError::Invalid("missing stored context"))?;
    if value.to_string().len() > MAX_BYTES {
        return Err(ContextError::Invalid("stored context exceeds its bound"));
    }
    decode(value.clone()).map(Some)
}

fn user_source_ids(
    connection: &Connection,
    session_id: &str,
    requested: Option<&str>,
    limit: i64,
) -> Result<Vec<String>, ContextError> {
    let mut query = connection
        .prepare(USER_SOURCE_IDS)
        .map_err(zuno_db::map_error)?;
    Ok(query
        .query_map(params![session_id, requested, limit], |row| row.get(0))
        .map_err(zuno_db::map_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(zuno_db::map_error)?)
}

fn user_source(
    connection: &Connection,
    session_id: &str,
    id: &str,
) -> Result<UserSource, ContextError> {
    if id.is_empty() || id.len() > 512 {
        return Err(ContextError::Invalid("invalid source ID"));
    }
    if user_source_ids(connection, session_id, Some(id), 1)?.is_empty() {
        return Err(ContextError::Invalid(
            "sources must be real user messages in this session",
        ));
    }
    let message = zuno_db::message::MessageStore::new(connection)
        .find_message(id)?
        .ok_or(ContextError::Invalid("source user message is missing"))?;
    if let Some(input) = zuno_db::inbox::input_for_message_in(connection, session_id, id)? {
        use zuno_types::execution::InputTriggerKind;
        if !matches!(
            input.trigger_kind,
            InputTriggerKind::User | InputTriggerKind::UserControl | InputTriggerKind::Legacy
        ) {
            return Err(ContextError::Invalid(
                "automatic reports are not user authorization",
            ));
        }
    }
    let source_bytes: i64 = connection
        .query_row(
            "SELECT COALESCE(SUM(length(CAST(data AS BLOB))),0) FROM part
         WHERE session_id=?1 AND message_id=?2",
            params![session_id, id],
            |row| row.get(0),
        )
        .map_err(zuno_db::map_error)?;
    if source_bytes > 65_536 {
        return Err(ContextError::Invalid("source parts exceed 64 KiB"));
    }
    let mut statement = connection
        .prepare("SELECT data FROM part WHERE session_id=?1 AND message_id=?2 ORDER BY id")
        .map_err(zuno_db::map_error)?;
    let parts = statement
        .query_map(params![session_id, id], |row| row.get::<_, String>(0))
        .map_err(zuno_db::map_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(zuno_db::map_error)?;
    Ok(UserSource {
        message_id: id.to_owned(),
        digest: sha256_json(&json!({
            "message":message.to_json(),"parts":parts,
        })),
        time_created: message.time_created,
    })
}

fn strings(values: &[String]) -> bool {
    values.len() <= 24
        && values
            .iter()
            .all(|v| !v.trim().is_empty() && v.len() <= 1024)
}

fn source_listing(
    connection: &Connection,
    session_id: &str,
    requested: &[String],
) -> Result<Vec<Value>, ContextError> {
    if requested.len() > 16 {
        return Err(ContextError::Invalid("too many requested sources"));
    }
    let ids = if requested.is_empty() {
        user_source_ids(connection, session_id, None, 16)?
    } else {
        requested.to_vec()
    };
    let mut result = Vec::new();
    let mut remaining = 12 * 1024_usize;
    for id in ids {
        let source = match user_source(connection, session_id, &id) {
            Ok(source) => source,
            Err(ContextError::Invalid(_)) if requested.is_empty() => continue,
            Err(error) => return Err(error),
        };
        let mut query = connection
            .prepare(
                "SELECT json_extract(data,'$.text') FROM part WHERE session_id=?1 AND message_id=?2
             AND json_extract(data,'$.type')='text' ORDER BY id",
            )
            .map_err(zuno_db::map_error)?;
        let pieces = query
            .query_map(params![session_id, id], |row| row.get::<_, String>(0))
            .map_err(zuno_db::map_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(zuno_db::map_error)?;
        let text = pieces.join("\n");
        let end = text.floor_char_boundary(text.len().min(2048).min(remaining));
        remaining = remaining.saturating_sub(end);
        result.push(json!({"source":source,"text":&text[..end],"truncated":end<text.len()}));
    }
    Ok(result)
}

fn validate(snapshot: &TaskContextSnapshot) -> Result<(), ContextError> {
    if snapshot.objective.trim().is_empty()
        || snapshot.objective.len() > 4096
        || !strings(&snapshot.authorized_actions)
        || !strings(&snapshot.prohibitions)
        || snapshot.sources.is_empty()
        || snapshot.sources.len() > 32
        || snapshot.checks.len() > 32
    {
        return Err(ContextError::Invalid(
            "objective, sources or lists exceed their bounds",
        ));
    }
    let mut ids = BTreeSet::new();
    for check in &snapshot.checks {
        if check.id.is_empty()
            || check.id.len() > 128
            || !ids.insert(&check.id)
            || check.description.trim().is_empty()
            || check.description.len() > 2048
            || !strings(&check.evidence)
            || (check.status == CheckStatus::Passed && check.evidence.is_empty())
        {
            return Err(ContextError::Invalid(
                "checks need stable unique IDs and passed checks need evidence",
            ));
        }
    }
    if let Some(blocker) = &snapshot.blocker {
        if blocker.reason.trim().is_empty()
            || blocker.reason.len() > 2048
            || blocker.reference.as_ref().is_some_and(|v| v.len() > 1024)
        {
            return Err(ContextError::Invalid("invalid blocker"));
        }
        if matches!(
            blocker.kind,
            BlockerKind::UserAuthority | BlockerKind::UserInformation
        ) && snapshot.decision_owner != DecisionOwner::User
        {
            return Err(ContextError::Invalid(
                "user-owned blockers must explicitly identify that owner",
            ));
        }
    }
    if snapshot.status == TaskStatus::Waiting && snapshot.blocker.is_none() {
        return Err(ContextError::Invalid(
            "waiting needs a real blocker, not a routine implementation choice",
        ));
    }
    if snapshot.status == TaskStatus::Complete
        && (snapshot.blocker.is_some()
            || snapshot.checks.is_empty()
            || snapshot
                .checks
                .iter()
                .any(|check| check.status != CheckStatus::Passed)
            || (snapshot.intent == TaskIntent::Delivery
                && !snapshot
                    .checks
                    .iter()
                    .any(|check| check.kind == CheckKind::Delivery)))
    {
        return Err(ContextError::Invalid(
            "delivery is incomplete; safety/negative checks do not substitute for successful deliverables",
        ));
    }
    if serde_json::to_vec(snapshot)
        .map_err(|_| ContextError::Invalid("cannot encode context"))?
        .len()
        > MAX_BYTES
    {
        return Err(ContextError::Invalid("context exceeds 24 KiB"));
    }
    Ok(())
}

fn update(
    pool: &Pool,
    session_id: &str,
    operation: &str,
    params: Params,
) -> Result<TaskContextSnapshot, ContextError> {
    pool.try_transaction(|tx| {
        let parent: Option<String> = tx
            .query_row(
                "SELECT parent_id FROM session WHERE id=?1",
                [session_id],
                |row| row.get(0),
            )
            .map_err(zuno_db::map_error)?;
        if parent.is_some() {
            return Err(ContextError::Invalid(
                "a child reports scope and cross-module decisions to its parent",
            ));
        }
        let digest = sha256_json(&json!(params));
        let previous_operation: Option<String> = tx.query_row(
            "SELECT data FROM event WHERE aggregate_id=?1 AND type='session.task_context.updated.1'
             AND json_extract(data,'$.operation')=?2 ORDER BY seq DESC LIMIT 1",
            params![session_id, operation], |row| row.get(0),
        ).optional().map_err(zuno_db::map_error)?;
        if let Some(raw) = previous_operation {
            let stored: Value = serde_json::from_str(&raw)
                .map_err(|_| ContextError::Invalid("corrupt operation"))?;
            if stored["argumentDigest"] != digest {
                return Err(ContextError::Conflict);
            }
            return decode(stored["context"].clone());
        }
        let previous = read_in(tx, session_id)?;
        if params.expected_revision != previous.as_ref().map(|c| c.revision) {
            return Err(ContextError::Conflict);
        }
        if params.source_message_ids.len() > 16 {
            return Err(ContextError::Invalid("too many sources"));
        }
        let mut sources = Vec::new();
        for id in &params.source_message_ids {
            let source = user_source(tx, session_id, id)?;
            if !sources
                .iter()
                .any(|s: &UserSource| s.message_id == source.message_id)
            {
                sources.push(source);
            }
        }
        let replacing = params.action == Action::Replace;
        let new_task = previous.is_none() || replacing;
        if new_task
            && (params.objective.is_none() || params.task_kind.is_none() || sources.is_empty())
        {
            return Err(ContextError::Invalid(
                "new tasks require an objective, intent and real user sources",
            ));
        }
        let scope_change = replacing
            || params.objective.is_some()
            || params.task_kind.is_some()
            || params.authorized_actions.is_some()
            || !params.prohibitions.is_empty()
            || params.revise_checks;
        if scope_change && sources.is_empty() {
            return Err(ContextError::Invalid(
                "scope changes require user source references",
            ));
        }
        let now = zuno_db::message::now_millis();
        let revision = previous
            .as_ref()
            .map_or(Some(1), |p| p.revision.checked_add(1))
            .ok_or(ContextError::Invalid("revision overflow"))?;
        let mut next = if new_task {
            TaskContextSnapshot {
                task_id: format!("taskctx_{}", uuid::Uuid::now_v7().simple()),
                revision,
                objective: String::new(),
                intent: TaskIntent::Delivery,
                authorized_actions: Vec::new(),
                prohibitions: Vec::new(),
                sources: Vec::new(),
                checks: Vec::new(),
                status: TaskStatus::Active,
                decision_owner: DecisionOwner::Agent,
                blocker: None,
                updated_at: now,
            }
        } else {
            previous.clone().expect("existing context")
        };
        next.revision = revision;
        next.updated_at = now;
        if let Some(objective) = params.objective {
            next.objective = objective;
        }
        if let Some(intent) = params.task_kind {
            next.intent = intent;
        }
        if let Some(actions) = params.authorized_actions {
            next.authorized_actions = actions;
        }
        for constraint in params.prohibitions {
            if !next.prohibitions.contains(&constraint) {
                next.prohibitions.push(constraint);
            }
        }
        for source in sources {
            if let Some(existing) = next
                .sources
                .iter_mut()
                .find(|s| s.message_id == source.message_id)
            {
                *existing = source;
            } else {
                next.sources.push(source);
            }
        }
        for check in params.checks {
            if let Some(existing) = next.checks.iter_mut().find(|c| c.id == check.id) {
                if existing.kind != check.kind {
                    return Err(ContextError::Invalid(
                        "a check cannot change between delivery and safety",
                    ));
                }
                if existing.description != check.description
                    && (!params.revise_checks
                        || check.status != CheckStatus::Pending
                        || !check.evidence.is_empty())
                {
                    return Err(ContextError::Invalid(
                        "check definition changes require revise_checks, user sources, pending status and fresh evidence",
                    ));
                }
                *existing = check;
            } else {
                next.checks.push(check);
            }
        }
        if let Some(status) = params.status {
            next.status = status;
            if status != TaskStatus::Waiting {
                next.blocker = None;
            }
        }
        if let Some(owner) = params.decision_owner {
            next.decision_owner = owner;
        }
        if let Some(blocker) = params.blocker {
            next.blocker = Some(blocker);
        }
        validate(&next)?;
        append_in(
            tx,
            session_id,
            NewSessionEvent::new(
                EVENT,
                json!({"operation":operation,"argumentDigest":digest,"context":next,
                "replacesTaskId":if replacing { previous.map(|p| p.task_id) } else { None },
                "authority":"user_sources_and_runtime_policy_only"})
                .as_object()
                .expect("object")
                .clone(),
            )?,
        )?;
        Ok(next)
    })
}

pub fn runtime_context(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<String>, ContextError> {
    let Some(context) = read_in(connection, session_id)? else {
        return Ok(None);
    };
    let newest = user_source_ids(connection, session_id, None, 1)?
        .into_iter()
        .next();
    let unincorporated = newest.filter(|id| {
        !context
            .sources
            .iter()
            .any(|source| &source.message_id == id)
    });
    let mut sources_current = true;
    for source in &context.sources {
        match user_source(connection, session_id, &source.message_id) {
            Ok(current) => sources_current &= current.digest == source.digest,
            Err(ContextError::Invalid(_)) => sources_current = false,
            Err(error) => return Err(error),
        }
    }
    Ok(Some(format!(
        "runtime.task_context: assistant-maintained task understanding, NOT a permission grant, \
         Goal resume or automatic wake. Current user instructions and native policy take precedence. \
         A new restriction refines unrevoked intent; an explicit inspection-only/new-task request must \
         not inherit execution authority. Passed safety checks do not prove successful delivery. \
         Reconcile newer user input before side effects.\n{}",
        json!({"context":context,"newerUserMessageId":unincorporated,
            "sourcesCurrent":sources_current,
            "historicalOnly":!sources_current || context.status == TaskStatus::Complete})
    )))
}

#[async_trait]
impl TypedTool for TaskContextTool {
    type Params = Params;
    fn id(&self) -> &str {
        ID
    }
    fn description(&self) -> &str {
        DESCRIPTION
    }
    fn history_policy(&self) -> HistoryPolicy {
        HistoryPolicy::AuthoritativeState
    }
    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Safe
    }
    fn effect(&self, args: &Value) -> ToolEffect {
        if args["action"] == "get" {
            ToolEffect::ReadOnly
        } else {
            ToolEffect::ManagedContext
        }
    }
    async fn run(&self, params: Params, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        ctx.ask(
            ID,
            PermissionAsk {
                permission: ID.to_owned(),
                patterns: vec!["*".to_owned()],
                always: vec!["*".to_owned()],
                ..Default::default()
            },
        )
        .await?;
        let pool = self.pool.clone();
        let changed = params.action != Action::Get;
        let result = tokio::task::spawn_blocking(move || {
            if !changed {
                let connection = pool.get()?;
                let context = read_in(&connection, &ctx.session_id)?;
                let sources =
                    source_listing(&connection, &ctx.session_id, &params.source_message_ids)?;
                Ok((context, sources))
            } else {
                let operation = format!("{}:{}", ctx.message_id, ctx.call_id);
                update(&pool, &ctx.session_id, &operation, params)
                    .map(|value| (Some(value), Vec::new()))
            }
        })
        .await
        .map_err(|error| ToolError::Failed {
            tool: ID.to_owned(),
            source: Box::new(error),
        })?
        .map_err(|error| match error {
            ContextError::Database(error) if error.is_retryable() => ToolError::Transient {
                tool: ID.to_owned(),
                retry_after: error.retry_after(),
                source: Box::new(error),
            },
            ContextError::Database(error) => ToolError::Failed {
                tool: ID.to_owned(),
                source: Box::new(error),
            },
            error => ToolError::InvalidArgs {
                tool: ID.to_owned(),
                source: Box::new(error),
            },
        })?;
        let (context, sources) = result;
        let output = ToolOutput::text(
            "Task context",
            json!({
                "authority":"user_sources_and_runtime_policy_only","context":context,
                "userSources":sources,
            })
            .to_string(),
        );
        Ok(if changed {
            output.with_dynamic_context_refresh(ToolDynamicContextRefresh::TaskContext)
        } else {
            output
        })
    }
}

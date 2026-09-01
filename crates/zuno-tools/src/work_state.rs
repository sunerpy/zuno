//! Durable Goal-linked plans and work items with optimistic concurrency.

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use zuno_db::Pool;
use zuno_error::{DbError, ToolError};
use zuno_tool::{
    PermissionAsk, Tool, ToolConcurrencyPolicy, ToolContext, ToolEffect, ToolOutput,
    ToolReplayPolicy, TypedTool, erase,
};

pub const PLAN_GET_TOOL_ID: &str = "plan_get";
pub const PLAN_UPDATE_TOOL_ID: &str = "plan_update";
pub const TODO_GET_TOOL_ID: &str = "todo_get";
pub const TODO_UPDATE_TOOL_ID: &str = "todo_update";

pub const PLAN_GET_DESCRIPTION: &str = include_str!("description/plan-get.txt");
pub const PLAN_UPDATE_DESCRIPTION: &str = include_str!("description/plan-update.txt");
pub const TODO_GET_DESCRIPTION: &str = include_str!("description/todo-get.txt");
pub const TODO_UPDATE_DESCRIPTION: &str = include_str!("description/todo-update.txt");

// Runtime work-state projections have a 16 KiB total envelope. Generated Zuno
// identifiers are much smaller; this ceiling admits external Unicode ids while
// ensuring one non-droppable identity cannot consume the whole envelope.
const MAX_DURABLE_IDENTIFIER_BYTES: usize = 512;
// Projection diagnostics occupy the goal_id slot so existing consumers cannot
// mistake a truncated value for the durable identity. The reserved prefix keeps
// user-written ids disjoint from host-generated omission/error markers.
const INVALID_DURABLE_IDENTIFIER_PREFIX: &str = "zuno.invalid-id/v1;";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
    Blocked,
}

impl WorkItemStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Blocked => "blocked",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            "cancelled" => Some(Self::Cancelled),
            "blocked" => Some(Self::Blocked),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemPriority {
    High,
    Medium,
    Low,
}

impl WorkItemPriority {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "high" => Some(Self::High),
            "medium" => Some(Self::Medium),
            "low" => Some(Self::Low),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Completed,
    Superseded,
}

impl PlanStepStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Superseded => "superseded",
        }
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Superseded)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    pub id: String,
    pub title: String,
    pub status: PlanStepStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkPlan {
    pub id: String,
    pub session_id: String,
    pub parent_plan_id: Option<String>,
    pub stack_depth: i64,
    pub goal_id: Option<String>,
    pub revision: i64,
    pub title: String,
    pub steps: Vec<PlanStep>,
    pub time_created: i64,
    pub time_updated: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: String,
    pub session_id: String,
    pub goal_id: Option<String>,
    pub plan_step_id: Option<String>,
    pub parent_id: Option<String>,
    pub subject: String,
    pub description: String,
    pub active_form: Option<String>,
    pub status: WorkItemStatus,
    pub priority: WorkItemPriority,
    pub dependencies: Vec<String>,
    pub owner: Option<String>,
    pub revision: i64,
    pub tokens_used: i64,
    pub usage_known: bool,
    pub time_used_ms: i64,
    pub time_created: i64,
    pub time_updated: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkStateSnapshot {
    pub plan: Option<WorkPlan>,
    pub items: Vec<WorkItem>,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkStateError {
    #[error(transparent)]
    Database(#[from] DbError),
    #[error("invalid work state: {0}")]
    Invalid(String),
    #[error("{kind} `{id}` does not exist")]
    NotFound { kind: &'static str, id: String },
    #[error("{kind} `{id}` revision conflict: expected {expected}, current {actual}")]
    RevisionConflict {
        kind: &'static str,
        id: String,
        expected: i64,
        actual: i64,
    },
}

impl WorkStateError {
    fn correctable(&self) -> bool {
        !matches!(self, Self::Database(_))
    }
}

pub trait WorkStateObserver: Send + Sync {
    fn changed(&self);
}

#[derive(Clone)]
pub struct WorkStateStore {
    pool: Arc<Pool>,
    observer: Option<Arc<dyn WorkStateObserver>>,
}

impl fmt::Debug for WorkStateStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkStateStore")
            .field("observer", &self.observer.as_ref().map(|_| "configured"))
            .finish_non_exhaustive()
    }
}

impl WorkStateStore {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self {
            pool,
            observer: None,
        }
    }

    #[must_use]
    pub fn with_observer(mut self, observer: Arc<dyn WorkStateObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    fn notify_changed(&self) {
        if let Some(observer) = &self.observer {
            observer.changed();
        }
    }

    pub fn snapshot(&self, session_id: &str) -> Result<WorkStateSnapshot, WorkStateError> {
        let connection = self.pool.get()?;
        Self::snapshot_in(&connection, session_id)
    }

    /// Read one complete plan/todo snapshot through a caller-owned SQLite snapshot.
    ///
    /// Runtime prompt assembly uses this together with jobs, inbox reports, and
    /// prompt receipts inside one deferred transaction so compaction cannot combine
    /// state from different commits.
    pub fn snapshot_in(
        connection: &zuno_db::Connection,
        session_id: &str,
    ) -> Result<WorkStateSnapshot, WorkStateError> {
        Ok(WorkStateSnapshot {
            plan: plan_in(connection, session_id)?.map(project_plan_for_snapshot),
            items: list_items_in(connection, session_id)?,
        })
    }

    pub fn plan(&self, session_id: &str) -> Result<Option<WorkPlan>, WorkStateError> {
        let connection = self.pool.get()?;
        plan_in(&connection, session_id).map_err(Into::into)
    }

    pub fn update_plan(
        &self,
        session_id: &str,
        params: PlanUpdateParams,
    ) -> Result<WorkPlan, WorkStateError> {
        self.update_plan_with_policy(session_id, params, false)
    }

    /// Apply one model-facing Plan operation without requiring a full snapshot.
    ///
    /// Reads are followed by revision-guarded writes, so a concurrent commit cannot
    /// turn a locally-derived patch into a lost update.
    pub fn mutate_plan(
        &self,
        session_id: &str,
        mutation: PlanMutationParams,
    ) -> Result<WorkPlan, WorkStateError> {
        let result = match mutation {
            PlanMutationParams::Create {
                expected_revision,
                goal_id,
                title,
                steps,
            } => {
                let current = self.plan(session_id)?;
                let steps = materialize_plan_steps(steps)?;
                let params = PlanUpdateParams {
                    expected_revision,
                    goal_id: goal_id.or_else(|| {
                        current
                            .as_ref()
                            .and_then(|plan| plan.goal_id.as_ref())
                            .cloned()
                    }),
                    title,
                    steps,
                };
                if current.is_some() {
                    self.replace_plan_for_objective(session_id, params)
                } else {
                    self.update_plan(session_id, params)
                }
            }
            PlanMutationParams::Patch {
                expected_revision,
                title,
                steps,
            } => {
                if title.is_none() && steps.is_empty() {
                    return Err(WorkStateError::Invalid(
                        "patch must change the plan title or at least one step".to_owned(),
                    ));
                }
                let current = self.plan_at_revision(session_id, expected_revision)?;
                let mut candidate = current.steps.clone();
                let mut patched_ids = BTreeSet::new();
                for patch in steps {
                    if patch.title.is_none() && patch.status.is_none() {
                        return Err(WorkStateError::Invalid(format!(
                            "plan step patch `{}` must change title or status",
                            patch.id
                        )));
                    }
                    if !patched_ids.insert(patch.id.clone()) {
                        return Err(WorkStateError::Invalid(format!(
                            "duplicate plan step patch `{}`",
                            patch.id
                        )));
                    }
                    let step = candidate
                        .iter_mut()
                        .find(|step| step.id == patch.id)
                        .ok_or_else(|| WorkStateError::NotFound {
                            kind: "plan step",
                            id: patch.id.clone(),
                        })?;
                    if let Some(title) = patch.title {
                        step.title = title;
                    }
                    if let Some(status) = patch.status {
                        step.status = status;
                    }
                }
                let title = title.unwrap_or_else(|| current.title.clone());
                if title == current.title && candidate == current.steps {
                    return Err(WorkStateError::Invalid(
                        "patch does not change the durable plan".to_owned(),
                    ));
                }
                self.update_plan(
                    session_id,
                    PlanUpdateParams {
                        expected_revision: Some(expected_revision),
                        goal_id: current.goal_id,
                        title,
                        steps: candidate,
                    },
                )
            }
            PlanMutationParams::Append {
                expected_revision,
                steps,
            } => {
                let current = self.plan_at_revision(session_id, expected_revision)?;
                let mut candidate = current.steps.clone();
                candidate.extend(materialize_plan_steps(steps)?);
                self.update_plan(
                    session_id,
                    PlanUpdateParams {
                        expected_revision: Some(expected_revision),
                        goal_id: current.goal_id,
                        title: current.title,
                        steps: candidate,
                    },
                )
            }
            PlanMutationParams::Push {
                expected_revision,
                title,
                steps,
            } => {
                let current = self.plan_at_revision(session_id, expected_revision)?;
                self.push_plan(
                    session_id,
                    PlanUpdateParams {
                        expected_revision: Some(expected_revision),
                        goal_id: current.goal_id,
                        title,
                        steps: materialize_plan_steps(steps)?,
                    },
                )
            }
            PlanMutationParams::Pop { expected_revision } => {
                let current = self.plan_at_revision(session_id, expected_revision)?;
                self.complete_subplan(
                    session_id,
                    PlanUpdateParams {
                        expected_revision: Some(expected_revision),
                        goal_id: current.goal_id,
                        title: current.title,
                        steps: current.steps,
                    },
                )
            }
        };
        if result.is_ok() {
            self.notify_changed();
        }
        result
    }

    fn plan_at_revision(
        &self,
        session_id: &str,
        expected_revision: i64,
    ) -> Result<WorkPlan, WorkStateError> {
        if expected_revision <= 0 {
            return Err(WorkStateError::Invalid(
                "expected_revision must be positive".to_owned(),
            ));
        }
        let current = self
            .plan(session_id)?
            .ok_or_else(|| WorkStateError::NotFound {
                kind: "plan",
                id: session_id.to_owned(),
            })?;
        if current.revision != expected_revision {
            return Err(WorkStateError::RevisionConflict {
                kind: "plan",
                id: current.id,
                expected: expected_revision,
                actual: current.revision,
            });
        }
        Ok(current)
    }

    /// Suspend the active Plan and install a focused child Plan atomically.
    ///
    /// The parent remains durable in `work_plan_archive` and is restored by
    /// [`Self::complete_subplan`]. A process restart therefore observes the same active
    /// child and stack depth rather than reconstructing nesting from model prose.
    pub fn push_plan(
        &self,
        session_id: &str,
        params: PlanUpdateParams,
    ) -> Result<WorkPlan, WorkStateError> {
        validate_plan(&params)?;
        let steps_json = serde_json::to_string(&params.steps)
            .map_err(|error| WorkStateError::Invalid(error.to_string()))?;
        let now = zuno_db::message::now_millis();
        let child_id = format!("plan_{}", uuid::Uuid::new_v4().simple());
        self.pool.try_transaction(|tx| {
            let current = plan_in(tx, session_id)?.ok_or_else(|| WorkStateError::NotFound {
                kind: "plan",
                id: session_id.to_owned(),
            })?;
            require_plan_revision(&current, params.expected_revision)?;
            archive_plan(tx, &current, ArchivedPlanState::Suspended, now)?;
            tx.execute("DELETE FROM work_plan WHERE session_id=?1", [session_id])
                .map_err(zuno_db::map_error)?;
            tx.execute(
                "INSERT INTO work_plan \
                 (session_id,id,parent_plan_id,stack_depth,goal_id,revision,title,steps,\
                  time_created,time_updated) \
                 VALUES (?1,?2,?3,?4,?5,1,?6,?7,?8,?8)",
                params![
                    session_id,
                    child_id,
                    current.id,
                    current.stack_depth.saturating_add(1),
                    params.goal_id.or(current.goal_id),
                    params.title,
                    steps_json,
                    now
                ],
            )
            .map_err(zuno_db::map_error)?;
            Ok(plan_in(tx, session_id)?.expect("child plan was inserted"))
        })
    }

    /// Complete the active child Plan and restore its suspended parent atomically.
    pub fn complete_subplan(
        &self,
        session_id: &str,
        params: PlanUpdateParams,
    ) -> Result<WorkPlan, WorkStateError> {
        validate_plan(&params)?;
        if params.steps.iter().any(|step| !step.status.is_terminal()) {
            return Err(WorkStateError::Invalid(
                "a subplan can return to its parent only after every child step is completed"
                    .to_owned(),
            ));
        }
        let steps_json = serde_json::to_string(&params.steps)
            .map_err(|error| WorkStateError::Invalid(error.to_string()))?;
        let now = zuno_db::message::now_millis();
        self.pool.try_transaction(|tx| {
            let current = plan_in(tx, session_id)?.ok_or_else(|| WorkStateError::NotFound {
                kind: "plan",
                id: session_id.to_owned(),
            })?;
            require_plan_revision(&current, params.expected_revision)?;
            let parent_id = current.parent_plan_id.clone().ok_or_else(|| {
                WorkStateError::Invalid("the active plan has no suspended parent".to_owned())
            })?;
            validate_plan_transition(tx, session_id, &current, &params, false)?;
            let completed = WorkPlan {
                revision: current.revision.saturating_add(1),
                title: params.title,
                steps: params.steps,
                goal_id: params.goal_id.or(current.goal_id),
                time_updated: now,
                ..current
            };
            archive_plan_with_steps(
                tx,
                &completed,
                &steps_json,
                ArchivedPlanState::Completed,
                now,
            )?;
            let parent = archived_plan_in(tx, session_id, &parent_id)?.ok_or_else(|| {
                WorkStateError::Invalid(format!("suspended parent plan `{parent_id}` is missing"))
            })?;
            tx.execute("DELETE FROM work_plan WHERE session_id=?1", [session_id])
                .map_err(zuno_db::map_error)?;
            tx.execute(
                "DELETE FROM work_plan_archive \
                 WHERE session_id=?1 AND id=?2 AND state='suspended'",
                params![session_id, parent_id],
            )
            .map_err(zuno_db::map_error)?;
            insert_active_plan(tx, &parent)?;
            Ok(plan_in(tx, session_id)?.expect("parent plan was restored"))
        })
    }

    /// Archive the current Plan and start a new root Plan without appending old steps.
    ///
    /// This host-owned boundary is used for a genuinely new user objective. Suspended
    /// ancestors are superseded in the same transaction so a stale child stack cannot
    /// later resurrect work from the previous objective.
    pub fn replace_plan_for_objective(
        &self,
        session_id: &str,
        params: PlanUpdateParams,
    ) -> Result<WorkPlan, WorkStateError> {
        validate_plan(&params)?;
        let steps_json = serde_json::to_string(&params.steps)
            .map_err(|error| WorkStateError::Invalid(error.to_string()))?;
        let now = zuno_db::message::now_millis();
        let next_id = format!("plan_{}", uuid::Uuid::new_v4().simple());
        self.pool.try_transaction(|tx| {
            let Some(current) = plan_in(tx, session_id)? else {
                if params.expected_revision.is_some() {
                    return Err(WorkStateError::RevisionConflict {
                        kind: "plan",
                        id: next_id.clone(),
                        expected: params.expected_revision.unwrap_or_default(),
                        actual: 0,
                    });
                }
                insert_new_root(
                    tx,
                    session_id,
                    &next_id,
                    params.goal_id.as_deref(),
                    &params.title,
                    &steps_json,
                    now,
                )?;
                return Ok(plan_in(tx, session_id)?.expect("root plan was inserted"));
            };
            require_plan_revision(&current, params.expected_revision)?;
            let state = if current.steps.iter().all(|step| step.status.is_terminal()) {
                ArchivedPlanState::Completed
            } else {
                ArchivedPlanState::Superseded
            };
            archive_plan(tx, &current, state, now)?;
            tx.execute(
                "UPDATE work_plan_archive SET state='superseded',time_archived=?1 \
                 WHERE session_id=?2 AND state='suspended'",
                params![now, session_id],
            )
            .map_err(zuno_db::map_error)?;
            tx.execute("DELETE FROM work_plan WHERE session_id=?1", [session_id])
                .map_err(zuno_db::map_error)?;
            insert_new_root(
                tx,
                session_id,
                &next_id,
                params.goal_id.as_deref(),
                &params.title,
                &steps_json,
                now,
            )?;
            Ok(plan_in(tx, session_id)?.expect("replacement plan was inserted"))
        })
    }

    /// Reconcile a Plan after the user established a new durable Goal objective.
    ///
    /// This is the one explicit boundary allowed to supersede unfinished steps that
    /// still own live Jobs. It does not cancel or settle those Jobs; their ordinary
    /// durable lifecycle remains visible independently of the superseded Plan step.
    pub fn update_plan_for_goal_boundary(
        &self,
        session_id: &str,
        params: PlanUpdateParams,
    ) -> Result<WorkPlan, WorkStateError> {
        self.update_plan_with_policy(session_id, params, true)
    }

    fn update_plan_with_policy(
        &self,
        session_id: &str,
        params: PlanUpdateParams,
        allow_linked_job_supersession: bool,
    ) -> Result<WorkPlan, WorkStateError> {
        validate_plan(&params)?;
        let steps_json = serde_json::to_string(&params.steps)
            .map_err(|error| WorkStateError::Invalid(error.to_string()))?;
        let now = zuno_db::message::now_millis();
        let candidate_id = format!("plan_{}", uuid::Uuid::new_v4().simple());
        self.pool.try_transaction(|tx| {
            let current = plan_in(tx, session_id)?;
            match current {
                None => {
                    if let Some(expected) = params.expected_revision {
                        return Err(WorkStateError::RevisionConflict {
                            kind: "plan",
                            id: candidate_id.clone(),
                            expected,
                            actual: 0,
                        });
                    }
                    tx.execute(
                        "INSERT INTO work_plan \
                         (session_id,id,parent_plan_id,stack_depth,goal_id,revision,title,steps,\
                          time_created,time_updated) \
                         VALUES (?1,?2,NULL,0,?3,1,?4,?5,?6,?6)",
                        params![
                            session_id,
                            candidate_id,
                            params.goal_id,
                            params.title,
                            steps_json,
                            now
                        ],
                    )
                    .map_err(zuno_db::map_error)?;
                }
                Some(current) => {
                    let Some(expected) = params.expected_revision else {
                        return Err(WorkStateError::Invalid(
                            "expected_revision is required when updating an existing plan"
                                .to_owned(),
                        ));
                    };
                    if expected != current.revision {
                        return Err(WorkStateError::RevisionConflict {
                            kind: "plan",
                            id: current.id,
                            expected,
                            actual: current.revision,
                        });
                    }
                    validate_plan_transition(
                        tx,
                        session_id,
                        &current,
                        &params,
                        allow_linked_job_supersession,
                    )?;
                    let changed = tx
                        .execute(
                            "UPDATE work_plan SET goal_id=?1,title=?2,steps=?3,\
                             revision=revision+1,time_updated=?4 \
                             WHERE session_id=?5 AND revision=?6",
                            params![
                                params.goal_id,
                                params.title,
                                steps_json,
                                now,
                                session_id,
                                expected
                            ],
                        )
                        .map_err(zuno_db::map_error)?;
                    if changed != 1 {
                        let actual = plan_in(tx, session_id)?
                            .map(|plan| plan.revision)
                            .unwrap_or_default();
                        return Err(WorkStateError::RevisionConflict {
                            kind: "plan",
                            id: current.id,
                            expected,
                            actual,
                        });
                    }
                }
            }
            Ok(plan_in(tx, session_id)?.expect("plan was inserted or updated"))
        })
    }

    pub fn items(&self, session_id: &str) -> Result<Vec<WorkItem>, WorkStateError> {
        let connection = self.pool.get()?;
        list_items_in(&connection, session_id).map_err(Into::into)
    }

    pub fn update_items(
        &self,
        session_id: &str,
        changes: Vec<WorkItemChange>,
    ) -> Result<Vec<WorkItem>, WorkStateError> {
        if changes.is_empty() {
            return Err(WorkStateError::Invalid(
                "changes must contain at least one operation".to_owned(),
            ));
        }
        let now = zuno_db::message::now_millis();
        self.pool.try_transaction(|tx| {
            for change in &changes {
                apply_item_change(tx, session_id, change, now)?;
            }
            let items = list_items_in(tx, session_id)?;
            validate_item_graph(&items, plan_in(tx, session_id)?.as_ref())?;
            Ok(items)
        })
    }

    /// Advance one runtime-owned item without letting a model forge metering fields.
    ///
    /// Runtime transitions use the same optimistic revision guard as model updates, but
    /// only this path may write elapsed time and provider-confirmed token usage.
    pub fn transition_runtime_item(
        &self,
        session_id: &str,
        id: &str,
        expected_revision: i64,
        status: WorkItemStatus,
        elapsed_ms: Option<i64>,
        tokens_used: Option<i64>,
    ) -> Result<WorkItem, WorkStateError> {
        if expected_revision <= 0 {
            return Err(WorkStateError::Invalid(
                "expected_revision must be positive".to_owned(),
            ));
        }
        if elapsed_ms.is_some_and(|value| value < 0) || tokens_used.is_some_and(|value| value < 0) {
            return Err(WorkStateError::Invalid(
                "runtime usage values must not be negative".to_owned(),
            ));
        }
        let now = zuno_db::message::now_millis();
        self.pool.try_transaction(|tx| {
            let current = list_items_in(tx, session_id)?
                .into_iter()
                .find(|item| item.id == id)
                .ok_or_else(|| WorkStateError::NotFound {
                    kind: "work item",
                    id: id.to_owned(),
                })?;
            if current.revision != expected_revision {
                return Err(WorkStateError::RevisionConflict {
                    kind: "work item",
                    id: id.to_owned(),
                    expected: expected_revision,
                    actual: current.revision,
                });
            }
            if !runtime_transition_allowed(current.status, status) {
                return Err(WorkStateError::Invalid(format!(
                    "runtime work item `{id}` cannot move from `{}` to `{}`",
                    current.status.as_str(),
                    status.as_str()
                )));
            }
            let changed = match elapsed_ms {
                Some(elapsed_ms) => tx
                    .execute(
                        "UPDATE work_item SET status=?1,\
                         active_form=CASE WHEN ?2 THEN NULL ELSE active_form END,\
                         revision=revision+1,tokens_used=?3,usage_known=?4,time_used_ms=?5,\
                         time_updated=?6 WHERE id=?7 AND session_id=?8 AND revision=?9",
                        params![
                            status.as_str(),
                            matches!(
                                status,
                                WorkItemStatus::Completed
                                    | WorkItemStatus::Cancelled
                                    | WorkItemStatus::Blocked
                            ),
                            tokens_used.unwrap_or_default(),
                            tokens_used.is_some(),
                            elapsed_ms,
                            now,
                            id,
                            session_id,
                            expected_revision
                        ],
                    )
                    .map_err(zuno_db::map_error)?,
                None => tx
                    .execute(
                        "UPDATE work_item SET status=?1,revision=revision+1,time_updated=?2 \
                         WHERE id=?3 AND session_id=?4 AND revision=?5",
                        params![status.as_str(), now, id, session_id, expected_revision],
                    )
                    .map_err(zuno_db::map_error)?,
            };
            if changed != 1 {
                return Err(item_write_error(tx, session_id, id, expected_revision)?);
            }
            list_items_in(tx, session_id)?
                .into_iter()
                .find(|item| item.id == id)
                .ok_or_else(|| WorkStateError::NotFound {
                    kind: "work item",
                    id: id.to_owned(),
                })
        })
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanUpdateParams {
    #[serde(default)]
    pub expected_revision: Option<i64>,
    #[serde(default)]
    pub goal_id: Option<String>,
    pub title: String,
    pub steps: Vec<PlanStep>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanStepInput {
    pub title: String,
    pub status: PlanStepStatus,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanStepPatch {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub status: Option<PlanStepStatus>,
}

/// Model-facing operation interface for durable Plans.
///
/// Internal host callers keep using [`PlanUpdateParams`] for atomic objective-boundary
/// transactions. Model calls never replace an existing snapshot: they name only the
/// fields or steps that changed, while the host owns newly-created step identifiers.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanMutationParams {
    Create {
        #[serde(default)]
        expected_revision: Option<i64>,
        #[serde(default)]
        goal_id: Option<String>,
        title: String,
        steps: Vec<PlanStepInput>,
    },
    Patch {
        expected_revision: i64,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        steps: Vec<PlanStepPatch>,
    },
    Append {
        expected_revision: i64,
        steps: Vec<PlanStepInput>,
    },
    Push {
        expected_revision: i64,
        title: String,
        steps: Vec<PlanStepInput>,
    },
    Pop {
        expected_revision: i64,
    },
}

impl PlanMutationParams {
    const fn result_title(&self) -> &'static str {
        match self {
            Self::Create { .. } => "Plan created",
            Self::Patch { .. } => "Plan patched",
            Self::Append { .. } => "Plan steps appended",
            Self::Push { .. } => "Subplan opened",
            Self::Pop { .. } => "Parent plan restored",
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkItemChange {
    Add {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        goal_id: Option<String>,
        #[serde(default)]
        plan_step_id: Option<String>,
        #[serde(default)]
        parent_id: Option<String>,
        subject: String,
        description: String,
        #[serde(default)]
        active_form: Option<String>,
        status: WorkItemStatus,
        priority: WorkItemPriority,
        #[serde(default)]
        dependencies: Vec<String>,
        #[serde(default)]
        owner: Option<String>,
    },
    Update {
        id: String,
        expected_revision: i64,
        #[serde(default)]
        goal_id: Option<String>,
        #[serde(default)]
        plan_step_id: Option<String>,
        #[serde(default)]
        parent_id: Option<String>,
        subject: String,
        description: String,
        #[serde(default)]
        active_form: Option<String>,
        status: WorkItemStatus,
        priority: WorkItemPriority,
        #[serde(default)]
        dependencies: Vec<String>,
        #[serde(default)]
        owner: Option<String>,
    },
    Remove {
        id: String,
        expected_revision: i64,
    },
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TodoUpdateParams {
    pub changes: Vec<WorkItemChange>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkStateGetParams {}

fn materialize_plan_steps(inputs: Vec<PlanStepInput>) -> Result<Vec<PlanStep>, WorkStateError> {
    if inputs.is_empty() {
        return Err(WorkStateError::Invalid(
            "plan operation must include at least one step".to_owned(),
        ));
    }
    Ok(inputs
        .into_iter()
        .map(|input| PlanStep {
            id: format!("step_{}", uuid::Uuid::new_v4().simple()),
            title: input.title,
            status: input.status,
        })
        .collect())
}

fn validate_plan(params: &PlanUpdateParams) -> Result<(), WorkStateError> {
    if params.title.trim().is_empty() {
        return Err(WorkStateError::Invalid(
            "plan title must not be empty".to_owned(),
        ));
    }
    if let Some(goal_id) = params.goal_id.as_deref() {
        validate_durable_identifier("plan goal_id", goal_id)?;
    }
    if params
        .expected_revision
        .is_some_and(|revision| revision <= 0)
    {
        return Err(WorkStateError::Invalid(
            "expected_revision must be positive".to_owned(),
        ));
    }
    let mut ids = BTreeSet::new();
    let mut pending = 0_usize;
    let mut in_progress = 0_usize;
    for step in &params.steps {
        if step.id.trim().is_empty() || step.title.trim().is_empty() {
            return Err(WorkStateError::Invalid(
                "plan step id and title must not be empty".to_owned(),
            ));
        }
        if !ids.insert(step.id.as_str()) {
            return Err(WorkStateError::Invalid(format!(
                "duplicate plan step id `{}`",
                step.id
            )));
        }
        match step.status {
            PlanStepStatus::Pending => pending += 1,
            PlanStepStatus::InProgress => in_progress += 1,
            PlanStepStatus::Completed | PlanStepStatus::Superseded => {}
        }
    }
    if in_progress > 1 {
        return Err(WorkStateError::Invalid(
            "at most one plan step may be in_progress".to_owned(),
        ));
    }
    if pending > 0 && in_progress != 1 {
        return Err(WorkStateError::Invalid(
            "pending plan steps require exactly one in_progress step".to_owned(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InvalidIdentifierReason {
    Empty,
    TooLong,
    SurroundingWhitespace,
    ControlCharacter,
    ReservedPrefix,
}

impl InvalidIdentifierReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too_long",
            Self::SurroundingWhitespace => "surrounding_whitespace",
            Self::ControlCharacter => "control_character",
            Self::ReservedPrefix => "reserved_prefix",
        }
    }
}

fn invalid_identifier_reason(value: &str) -> Option<InvalidIdentifierReason> {
    if value.is_empty() {
        return Some(InvalidIdentifierReason::Empty);
    }
    if value.len() > MAX_DURABLE_IDENTIFIER_BYTES {
        return Some(InvalidIdentifierReason::TooLong);
    }
    if value.starts_with(INVALID_DURABLE_IDENTIFIER_PREFIX) {
        return Some(InvalidIdentifierReason::ReservedPrefix);
    }
    if value.trim() != value {
        return Some(InvalidIdentifierReason::SurroundingWhitespace);
    }
    value
        .chars()
        .any(char::is_control)
        .then_some(InvalidIdentifierReason::ControlCharacter)
}

fn validate_durable_identifier(field: &str, value: &str) -> Result<(), WorkStateError> {
    let Some(reason) = invalid_identifier_reason(value) else {
        return Ok(());
    };
    Err(WorkStateError::Invalid(format!(
        "{field} is invalid ({reason}); identifiers must be non-empty, contain no surrounding \
         whitespace or control characters, avoid the reserved \
         `{INVALID_DURABLE_IDENTIFIER_PREFIX}` prefix, and use at most \
         {MAX_DURABLE_IDENTIFIER_BYTES} bytes; received {} bytes",
        value.len(),
        reason = reason.as_str()
    )))
}

fn project_plan_for_snapshot(mut plan: WorkPlan) -> WorkPlan {
    if let Some(goal_id) = plan.goal_id.as_mut()
        && let Some(reason) = invalid_identifier_reason(goal_id)
    {
        *goal_id = invalid_durable_identifier_projection("plan.goal_id", reason, goal_id);
    }
    plan
}

fn invalid_durable_identifier_projection(
    field: &str,
    reason: InvalidIdentifierReason,
    value: &str,
) -> String {
    // Preserve a stable correlation key for the exact durable bytes without
    // copying an unbounded value into the prompt or mutating SQLite.
    let digest = hex::encode(Sha256::digest(value.as_bytes()));
    let projection = format!(
        "{INVALID_DURABLE_IDENTIFIER_PREFIX}field={field};error={};value=omitted;bytes={};\
         sha256={digest}",
        reason.as_str(),
        value.len()
    );
    debug_assert!(projection.len() <= MAX_DURABLE_IDENTIFIER_BYTES);
    projection
}

fn validate_plan_transition(
    transaction: &Transaction<'_>,
    session_id: &str,
    current: &WorkPlan,
    candidate: &PlanUpdateParams,
    allow_linked_job_supersession: bool,
) -> Result<(), WorkStateError> {
    let candidate_statuses = candidate
        .steps
        .iter()
        .map(|step| (step.id.as_str(), step.status))
        .collect::<BTreeMap<_, _>>();
    for step in &current.steps {
        let Some(status) = candidate_statuses.get(step.id.as_str()).copied() else {
            return Err(WorkStateError::Invalid(format!(
                "existing plan step id `{}` must remain stable across revisions",
                step.id
            )));
        };
        if step.status.is_terminal() && status != step.status {
            return Err(WorkStateError::Invalid(format!(
                "terminal plan step `{}` cannot change from `{}` to `{}`",
                step.id,
                step.status.as_str(),
                status.as_str()
            )));
        }
        if !allow_linked_job_supersession
            && !step.status.is_terminal()
            && status.is_terminal()
            && let Some((job_id, job_status)) =
                blocking_job_for_plan_step(transaction, session_id, &current.id, &step.id)?
        {
            return Err(WorkStateError::Invalid(format!(
                "plan step `{}` cannot complete while linked job `{job_id}` is `{job_status}` \
                 or still has an unconsumed report; reconcile the durable job first",
                step.id
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum ArchivedPlanState {
    Suspended,
    Completed,
    Superseded,
}

impl ArchivedPlanState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Suspended => "suspended",
            Self::Completed => "completed",
            Self::Superseded => "superseded",
        }
    }
}

fn require_plan_revision(current: &WorkPlan, expected: Option<i64>) -> Result<(), WorkStateError> {
    let Some(expected) = expected else {
        return Err(WorkStateError::Invalid(
            "expected_revision is required when changing an existing plan".to_owned(),
        ));
    };
    if expected != current.revision {
        return Err(WorkStateError::RevisionConflict {
            kind: "plan",
            id: current.id.clone(),
            expected,
            actual: current.revision,
        });
    }
    Ok(())
}

fn archive_plan(
    transaction: &Transaction<'_>,
    plan: &WorkPlan,
    state: ArchivedPlanState,
    now: i64,
) -> Result<(), WorkStateError> {
    let steps = serde_json::to_string(&plan.steps)
        .map_err(|error| WorkStateError::Invalid(error.to_string()))?;
    archive_plan_with_steps(transaction, plan, &steps, state, now)
}

fn archive_plan_with_steps(
    transaction: &Transaction<'_>,
    plan: &WorkPlan,
    steps: &str,
    state: ArchivedPlanState,
    now: i64,
) -> Result<(), WorkStateError> {
    transaction
        .execute(
            "INSERT INTO work_plan_archive \
             (id,session_id,parent_plan_id,stack_depth,goal_id,revision,title,steps,state,\
              time_created,time_updated,time_archived) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                plan.id,
                plan.session_id,
                plan.parent_plan_id,
                plan.stack_depth,
                plan.goal_id,
                plan.revision,
                plan.title,
                steps,
                state.as_str(),
                plan.time_created,
                plan.time_updated,
                now
            ],
        )
        .map_err(zuno_db::map_error)?;
    Ok(())
}

fn insert_active_plan(
    transaction: &Transaction<'_>,
    plan: &WorkPlan,
) -> Result<(), WorkStateError> {
    let steps = serde_json::to_string(&plan.steps)
        .map_err(|error| WorkStateError::Invalid(error.to_string()))?;
    transaction
        .execute(
            "INSERT INTO work_plan \
             (session_id,id,parent_plan_id,stack_depth,goal_id,revision,title,steps,\
              time_created,time_updated) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                plan.session_id,
                plan.id,
                plan.parent_plan_id,
                plan.stack_depth,
                plan.goal_id,
                plan.revision,
                plan.title,
                steps,
                plan.time_created,
                plan.time_updated
            ],
        )
        .map_err(zuno_db::map_error)?;
    Ok(())
}

fn insert_new_root(
    transaction: &Transaction<'_>,
    session_id: &str,
    id: &str,
    goal_id: Option<&str>,
    title: &str,
    steps: &str,
    now: i64,
) -> Result<(), WorkStateError> {
    transaction
        .execute(
            "INSERT INTO work_plan \
             (session_id,id,parent_plan_id,stack_depth,goal_id,revision,title,steps,\
              time_created,time_updated) \
             VALUES (?1,?2,NULL,0,?3,1,?4,?5,?6,?6)",
            params![session_id, id, goal_id, title, steps, now],
        )
        .map_err(zuno_db::map_error)?;
    Ok(())
}

fn blocking_job_for_plan_step(
    transaction: &Transaction<'_>,
    session_id: &str,
    plan_id: &str,
    plan_step_id: &str,
) -> Result<Option<(String, String)>, WorkStateError> {
    transaction
        .query_row(
            "SELECT j.id, j.status FROM agent_job AS j \
             WHERE j.parent_session_id = ?1 \
               AND json_extract(j.subject_payload, '$.workContext.planId') = ?2 \
               AND json_extract(j.subject_payload, '$.workContext.planStepId') = ?3 \
               AND ( \
                 j.status IN ('queued', 'running', 'uncertain') \
                 OR EXISTS ( \
                   SELECT 1 FROM session_input AS i \
                   WHERE i.id = j.report_input_id \
                     AND i.session_id = j.parent_session_id \
                     AND i.state IN ('queued', 'steering', 'promoted') \
                 ) \
               ) \
             ORDER BY j.time_created, j.id LIMIT 1",
            params![session_id, plan_id, plan_step_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(zuno_db::map_error)
        .map_err(Into::into)
}

fn validate_item_fields(
    id: &str,
    subject: &str,
    description: &str,
    dependencies: &[String],
    expected_revision: Option<i64>,
) -> Result<(), WorkStateError> {
    if id.trim().is_empty() || subject.trim().is_empty() || description.trim().is_empty() {
        return Err(WorkStateError::Invalid(
            "work item id, subject, and description must not be empty".to_owned(),
        ));
    }
    if expected_revision.is_some_and(|revision| revision <= 0) {
        return Err(WorkStateError::Invalid(
            "expected_revision must be positive".to_owned(),
        ));
    }
    if dependencies.iter().any(|dependency| dependency == id) {
        return Err(WorkStateError::Invalid(format!(
            "work item `{id}` cannot depend on itself"
        )));
    }
    let unique = dependencies.iter().collect::<BTreeSet<_>>();
    if unique.len() != dependencies.len() {
        return Err(WorkStateError::Invalid(format!(
            "work item `{id}` contains duplicate dependencies"
        )));
    }
    Ok(())
}

fn apply_item_change(
    tx: &Transaction<'_>,
    session_id: &str,
    change: &WorkItemChange,
    now: i64,
) -> Result<(), WorkStateError> {
    match change {
        WorkItemChange::Add {
            id,
            goal_id,
            plan_step_id,
            parent_id,
            subject,
            description,
            active_form,
            status,
            priority,
            dependencies,
            owner,
        } => {
            let id = id
                .clone()
                .unwrap_or_else(|| format!("todo_{}", uuid::Uuid::new_v4().simple()));
            validate_item_fields(&id, subject, description, dependencies, None)?;
            let dependencies = serde_json::to_string(dependencies)
                .map_err(|error| WorkStateError::Invalid(error.to_string()))?;
            tx.execute(
                "INSERT INTO work_item \
                 (id,session_id,goal_id,plan_step_id,parent_id,subject,description,active_form,\
                  status,priority,dependencies,owner,revision,tokens_used,usage_known,time_used_ms,\
                  time_created,time_updated) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,1,0,0,0,?13,?13)",
                params![
                    id,
                    session_id,
                    goal_id,
                    plan_step_id,
                    parent_id,
                    subject,
                    description,
                    active_form,
                    status.as_str(),
                    priority.as_str(),
                    dependencies,
                    owner,
                    now
                ],
            )
            .map_err(zuno_db::map_error)?;
        }
        WorkItemChange::Update {
            id,
            expected_revision,
            goal_id,
            plan_step_id,
            parent_id,
            subject,
            description,
            active_form,
            status,
            priority,
            dependencies,
            owner,
        } => {
            validate_item_fields(
                id,
                subject,
                description,
                dependencies,
                Some(*expected_revision),
            )?;
            let dependencies = serde_json::to_string(dependencies)
                .map_err(|error| WorkStateError::Invalid(error.to_string()))?;
            let changed = tx
                .execute(
                    "UPDATE work_item SET goal_id=?1,plan_step_id=?2,parent_id=?3,subject=?4,\
                     description=?5,active_form=?6,status=?7,priority=?8,dependencies=?9,owner=?10,\
                     revision=revision+1,time_updated=?11 \
                     WHERE id=?12 AND session_id=?13 AND revision=?14",
                    params![
                        goal_id,
                        plan_step_id,
                        parent_id,
                        subject,
                        description,
                        active_form,
                        status.as_str(),
                        priority.as_str(),
                        dependencies,
                        owner,
                        now,
                        id,
                        session_id,
                        expected_revision
                    ],
                )
                .map_err(zuno_db::map_error)?;
            if changed != 1 {
                return Err(item_write_error(tx, session_id, id, *expected_revision)?);
            }
        }
        WorkItemChange::Remove {
            id,
            expected_revision,
        } => {
            if *expected_revision <= 0 {
                return Err(WorkStateError::Invalid(
                    "expected_revision must be positive".to_owned(),
                ));
            }
            let changed = tx
                .execute(
                    "DELETE FROM work_item WHERE id=?1 AND session_id=?2 AND revision=?3",
                    params![id, session_id, expected_revision],
                )
                .map_err(zuno_db::map_error)?;
            if changed != 1 {
                return Err(item_write_error(tx, session_id, id, *expected_revision)?);
            }
        }
    }
    Ok(())
}

fn item_write_error(
    tx: &Transaction<'_>,
    session_id: &str,
    id: &str,
    expected: i64,
) -> Result<WorkStateError, DbError> {
    let actual = tx
        .query_row(
            "SELECT revision FROM work_item WHERE id=?1 AND session_id=?2",
            params![id, session_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(zuno_db::map_error)?;
    Ok(match actual {
        Some(actual) => WorkStateError::RevisionConflict {
            kind: "work item",
            id: id.to_owned(),
            expected,
            actual,
        },
        None => WorkStateError::NotFound {
            kind: "work item",
            id: id.to_owned(),
        },
    })
}

fn validate_item_graph(items: &[WorkItem], plan: Option<&WorkPlan>) -> Result<(), WorkStateError> {
    let ids = items
        .iter()
        .map(|item| item.id.as_str())
        .collect::<BTreeSet<_>>();
    let plan_steps = plan
        .into_iter()
        .flat_map(|plan| plan.steps.iter().map(|step| step.id.as_str()))
        .collect::<BTreeSet<_>>();
    for item in items {
        if let Some(parent_id) = item.parent_id.as_deref()
            && !ids.contains(parent_id)
        {
            return Err(WorkStateError::Invalid(format!(
                "work item `{}` references missing parent `{parent_id}`",
                item.id
            )));
        }
        if let Some(step_id) = item.plan_step_id.as_deref()
            && !plan_steps.contains(step_id)
        {
            return Err(WorkStateError::Invalid(format!(
                "work item `{}` references missing plan step `{step_id}`",
                item.id
            )));
        }
        for dependency in &item.dependencies {
            if !ids.contains(dependency.as_str()) {
                return Err(WorkStateError::Invalid(format!(
                    "work item `{}` references missing dependency `{dependency}`",
                    item.id
                )));
            }
        }
    }
    validate_item_graph_is_acyclic(items)?;
    Ok(())
}

fn runtime_transition_allowed(from: WorkItemStatus, to: WorkItemStatus) -> bool {
    matches!(
        (from, to),
        (
            WorkItemStatus::Pending,
            WorkItemStatus::InProgress | WorkItemStatus::Cancelled | WorkItemStatus::Blocked
        ) | (
            WorkItemStatus::InProgress,
            WorkItemStatus::Completed | WorkItemStatus::Cancelled | WorkItemStatus::Blocked
        )
    )
}

fn validate_item_graph_is_acyclic(items: &[WorkItem]) -> Result<(), WorkStateError> {
    let graph = items
        .iter()
        .map(|item| {
            let mut edges = item
                .dependencies
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>();
            if let Some(parent_id) = item.parent_id.as_deref() {
                edges.push(parent_id);
            }
            (item.id.as_str(), edges)
        })
        .collect::<BTreeMap<_, _>>();
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for id in graph.keys().copied() {
        visit_item(id, &graph, &mut visiting, &mut visited)?;
    }
    Ok(())
}

fn visit_item<'a>(
    id: &'a str,
    graph: &BTreeMap<&'a str, Vec<&'a str>>,
    visiting: &mut BTreeSet<&'a str>,
    visited: &mut BTreeSet<&'a str>,
) -> Result<(), WorkStateError> {
    if visited.contains(id) {
        return Ok(());
    }
    if !visiting.insert(id) {
        return Err(WorkStateError::Invalid(format!(
            "work item graph contains a cycle through `{id}`"
        )));
    }
    if let Some(edges) = graph.get(id) {
        for next in edges {
            visit_item(next, graph, visiting, visited)?;
        }
    }
    visiting.remove(id);
    visited.insert(id);
    Ok(())
}

fn plan_in(connection: &Connection, session_id: &str) -> Result<Option<WorkPlan>, DbError> {
    let row = connection
        .query_row(
            "SELECT id,parent_plan_id,stack_depth,goal_id,revision,title,steps,\
                    time_created,time_updated \
             FROM work_plan WHERE session_id=?1",
            [session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            },
        )
        .optional()
        .map_err(zuno_db::map_error)?;
    let Some((
        id,
        parent_plan_id,
        stack_depth,
        goal_id,
        revision,
        title,
        steps,
        time_created,
        time_updated,
    )) = row
    else {
        return Ok(None);
    };
    let steps = serde_json::from_str(&steps).map_err(json_db_error)?;
    Ok(Some(WorkPlan {
        id,
        session_id: session_id.to_owned(),
        parent_plan_id,
        stack_depth,
        goal_id,
        revision,
        title,
        steps,
        time_created,
        time_updated,
    }))
}

fn archived_plan_in(
    connection: &Connection,
    session_id: &str,
    id: &str,
) -> Result<Option<WorkPlan>, DbError> {
    let row = connection
        .query_row(
            "SELECT parent_plan_id,stack_depth,goal_id,revision,title,steps,\
                    time_created,time_updated \
             FROM work_plan_archive \
             WHERE session_id=?1 AND id=?2 AND state='suspended'",
            params![session_id, id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            },
        )
        .optional()
        .map_err(zuno_db::map_error)?;
    let Some((
        parent_plan_id,
        stack_depth,
        goal_id,
        revision,
        title,
        steps,
        time_created,
        time_updated,
    )) = row
    else {
        return Ok(None);
    };
    Ok(Some(WorkPlan {
        id: id.to_owned(),
        session_id: session_id.to_owned(),
        parent_plan_id,
        stack_depth,
        goal_id,
        revision,
        title,
        steps: serde_json::from_str(&steps).map_err(json_db_error)?,
        time_created,
        time_updated,
    }))
}

fn list_items_in(connection: &Connection, session_id: &str) -> Result<Vec<WorkItem>, DbError> {
    let mut statement = connection
        .prepare(
            "SELECT id,goal_id,plan_step_id,parent_id,subject,description,active_form,status,\
             priority,dependencies,owner,revision,tokens_used,usage_known,time_used_ms,\
             time_created,time_updated FROM work_item WHERE session_id=?1 ORDER BY time_created,id",
        )
        .map_err(zuno_db::map_error)?;
    let mut rows = statement.query([session_id]).map_err(zuno_db::map_error)?;
    let mut items = Vec::new();
    while let Some(row) = rows.next().map_err(zuno_db::map_error)? {
        let status: String = row.get(7).map_err(zuno_db::map_error)?;
        let priority: String = row.get(8).map_err(zuno_db::map_error)?;
        let dependencies: String = row.get(9).map_err(zuno_db::map_error)?;
        items.push(WorkItem {
            id: row.get(0).map_err(zuno_db::map_error)?,
            session_id: session_id.to_owned(),
            goal_id: row.get(1).map_err(zuno_db::map_error)?,
            plan_step_id: row.get(2).map_err(zuno_db::map_error)?,
            parent_id: row.get(3).map_err(zuno_db::map_error)?,
            subject: row.get(4).map_err(zuno_db::map_error)?,
            description: row.get(5).map_err(zuno_db::map_error)?,
            active_form: row.get(6).map_err(zuno_db::map_error)?,
            status: WorkItemStatus::parse(&status)
                .ok_or_else(|| json_db_error(format!("unknown work item status `{status}`")))?,
            priority: WorkItemPriority::parse(&priority)
                .ok_or_else(|| json_db_error(format!("unknown work item priority `{priority}`")))?,
            dependencies: serde_json::from_str(&dependencies).map_err(json_db_error)?,
            owner: row.get(10).map_err(zuno_db::map_error)?,
            revision: row.get(11).map_err(zuno_db::map_error)?,
            tokens_used: row.get(12).map_err(zuno_db::map_error)?,
            usage_known: row.get(13).map_err(zuno_db::map_error)?,
            time_used_ms: row.get(14).map_err(zuno_db::map_error)?,
            time_created: row.get(15).map_err(zuno_db::map_error)?,
            time_updated: row.get(16).map_err(zuno_db::map_error)?,
        });
    }
    Ok(items)
}

fn json_db_error(error: impl std::fmt::Display) -> DbError {
    DbError::Query {
        source: Box::new(std::io::Error::other(error.to_string())),
    }
}

#[derive(Debug, Clone)]
pub struct PlanGetTool(WorkStateStore);
#[derive(Debug, Clone)]
pub struct PlanUpdateTool(WorkStateStore);
#[derive(Debug, Clone)]
pub struct TodoGetTool(WorkStateStore);
#[derive(Debug, Clone)]
pub struct TodoUpdateTool(WorkStateStore);

impl PlanGetTool {
    #[must_use]
    pub fn new(store: WorkStateStore) -> Self {
        Self(store)
    }
}
impl PlanUpdateTool {
    #[must_use]
    pub fn new(store: WorkStateStore) -> Self {
        Self(store)
    }
}
impl TodoGetTool {
    #[must_use]
    pub fn new(store: WorkStateStore) -> Self {
        Self(store)
    }
}
impl TodoUpdateTool {
    #[must_use]
    pub fn new(store: WorkStateStore) -> Self {
        Self(store)
    }
}

#[async_trait]
impl TypedTool for PlanGetTool {
    type Params = WorkStateGetParams;
    fn id(&self) -> &str {
        PLAN_GET_TOOL_ID
    }
    fn description(&self) -> &str {
        PLAN_GET_DESCRIPTION
    }
    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Safe
    }
    fn effect(&self, _args: &Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }
    fn concurrency_policy(&self) -> ToolConcurrencyPolicy {
        ToolConcurrencyPolicy::ParallelSafe
    }
    async fn run(&self, _params: Self::Params, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let store = self.0.clone();
        let session_id = ctx.session_id;
        let plan = tokio::task::spawn_blocking(move || store.plan(&session_id))
            .await
            .map_err(|error| failed(PLAN_GET_TOOL_ID, error))?
            .map_err(|error| map_error(PLAN_GET_TOOL_ID, error))?;
        output(PLAN_GET_TOOL_ID, "Plan", "plan", plan)
    }
}

#[async_trait]
impl TypedTool for PlanUpdateTool {
    type Params = PlanMutationParams;
    fn id(&self) -> &str {
        PLAN_UPDATE_TOOL_ID
    }
    fn description(&self) -> &str {
        PLAN_UPDATE_DESCRIPTION
    }
    fn effect(&self, _args: &Value) -> ToolEffect {
        ToolEffect::SideEffecting
    }
    async fn run(&self, params: Self::Params, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        authorize(&ctx, PLAN_UPDATE_TOOL_ID).await?;
        let store = self.0.clone();
        let session_id = ctx.session_id;
        let title = params.result_title();
        let plan = tokio::task::spawn_blocking(move || store.mutate_plan(&session_id, params))
            .await
            .map_err(|error| failed(PLAN_UPDATE_TOOL_ID, error))?
            .map_err(|error| map_error(PLAN_UPDATE_TOOL_ID, error))?;
        output(PLAN_UPDATE_TOOL_ID, title, "plan", Some(plan))
    }
}

#[async_trait]
impl TypedTool for TodoGetTool {
    type Params = WorkStateGetParams;
    fn id(&self) -> &str {
        TODO_GET_TOOL_ID
    }
    fn description(&self) -> &str {
        TODO_GET_DESCRIPTION
    }
    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Safe
    }
    fn effect(&self, _args: &Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }
    fn concurrency_policy(&self) -> ToolConcurrencyPolicy {
        ToolConcurrencyPolicy::ParallelSafe
    }
    async fn run(&self, _params: Self::Params, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let store = self.0.clone();
        let session_id = ctx.session_id;
        let items = tokio::task::spawn_blocking(move || store.items(&session_id))
            .await
            .map_err(|error| failed(TODO_GET_TOOL_ID, error))?
            .map_err(|error| map_error(TODO_GET_TOOL_ID, error))?;
        output(
            TODO_GET_TOOL_ID,
            &format!("{} work items", items.len()),
            "todos",
            items,
        )
    }
}

#[async_trait]
impl TypedTool for TodoUpdateTool {
    type Params = TodoUpdateParams;
    fn id(&self) -> &str {
        TODO_UPDATE_TOOL_ID
    }
    fn description(&self) -> &str {
        TODO_UPDATE_DESCRIPTION
    }
    fn effect(&self, _args: &Value) -> ToolEffect {
        ToolEffect::SideEffecting
    }
    async fn run(&self, params: Self::Params, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        authorize(&ctx, TODO_UPDATE_TOOL_ID).await?;
        let store = self.0.clone();
        let session_id = ctx.session_id;
        let items =
            tokio::task::spawn_blocking(move || store.update_items(&session_id, params.changes))
                .await
                .map_err(|error| failed(TODO_UPDATE_TOOL_ID, error))?
                .map_err(|error| map_error(TODO_UPDATE_TOOL_ID, error))?;
        output(
            TODO_UPDATE_TOOL_ID,
            &format!("{} work items", items.len()),
            "todos",
            items,
        )
    }
}

pub fn work_state_tools(pool: Arc<Pool>) -> Vec<Arc<dyn Tool>> {
    work_state_tools_from_store(WorkStateStore::new(pool))
}

pub fn work_state_tools_with_observer(
    pool: Arc<Pool>,
    observer: Arc<dyn WorkStateObserver>,
) -> Vec<Arc<dyn Tool>> {
    work_state_tools_from_store(WorkStateStore::new(pool).with_observer(observer))
}

fn work_state_tools_from_store(store: WorkStateStore) -> Vec<Arc<dyn Tool>> {
    vec![
        erase(PlanGetTool::new(store.clone())),
        erase(PlanUpdateTool::new(store.clone())),
        erase(TodoGetTool::new(store.clone())),
        erase(TodoUpdateTool::new(store)),
    ]
}

async fn authorize(ctx: &ToolContext, tool: &str) -> Result<(), ToolError> {
    ctx.ask(
        tool,
        PermissionAsk {
            permission: tool.to_owned(),
            patterns: vec!["*".to_owned()],
            metadata: Map::new(),
            always: vec!["*".to_owned()],
            ..PermissionAsk::default()
        },
    )
    .await
}

fn output<T: Serialize>(
    tool: &str,
    title: &str,
    key: &str,
    value: T,
) -> Result<ToolOutput, ToolError> {
    let value = serde_json::to_value(value).map_err(|error| failed(tool, error))?;
    let rendered = serde_json::to_string_pretty(&value).map_err(|error| failed(tool, error))?;
    Ok(ToolOutput::text(title, rendered).with_metadata(key, value))
}

fn map_error(tool: &str, error: WorkStateError) -> ToolError {
    if error.correctable() {
        ToolError::InvalidArgs {
            tool: tool.to_owned(),
            source: Box::new(error),
        }
    } else {
        ToolError::Failed {
            tool: tool.to_owned(),
            source: Box::new(error),
        }
    }
}

fn failed(tool: &str, error: impl std::error::Error + Send + Sync + 'static) -> ToolError {
    ToolError::Failed {
        tool: tool.to_owned(),
        source: Box::new(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn initialize_store(location: &zuno_paths::DbLocation) -> WorkStateStore {
        let pool = Arc::new(Pool::open(location).expect("open work-state database"));
        let mut connection = pool.open_connection().expect("open connection");
        zuno_db::migration::apply(&mut connection).expect("apply schema");
        connection
            .execute(
                "INSERT INTO project \
                 (id,worktree,vcs,name,icon_url,icon_url_override,icon_color,time_created,\
                  time_updated,time_initialized,sandboxes,commands) \
                 VALUES ('prj','/tmp',NULL,NULL,NULL,NULL,NULL,1,1,NULL,'[]',NULL)",
                [],
            )
            .expect("insert project");
        connection
            .execute(
                "INSERT INTO session \
                 (id,project_id,slug,directory,title,version,time_created,time_updated) \
                 VALUES ('ses','prj','ses','/tmp','session','test',1,1)",
                [],
            )
            .expect("insert session");
        WorkStateStore::new(pool)
    }

    fn store() -> WorkStateStore {
        initialize_store(&zuno_paths::DbLocation::Memory)
    }

    fn step(id: &str, status: PlanStepStatus) -> PlanStep {
        PlanStep {
            id: id.to_owned(),
            title: format!("step {id}"),
            status,
        }
    }

    #[test]
    fn plan_steps_reject_work_item_only_terminal_states() {
        for status in ["blocked", "cancelled"] {
            let decoded = serde_json::from_value::<PlanUpdateParams>(serde_json::json!({
                "title": "ship",
                "steps": [{
                    "id": "verify",
                    "title": "verify release",
                    "status": status,
                }]
            }));
            assert!(
                decoded.is_err(),
                "plan status `{status}` is not representable by stable ACP and must be rejected"
            );
        }

        serde_json::from_value::<PlanUpdateParams>(serde_json::json!({
            "title": "ship",
            "steps": [{"id":"verify","title":"verify release","status":"completed"}]
        }))
        .expect("the three ACP execution states remain valid");
    }

    #[test]
    fn plan_snapshots_enforce_execution_progress_invariants() {
        let valid = PlanUpdateParams {
            expected_revision: None,
            goal_id: None,
            title: "release".to_owned(),
            steps: vec![
                step("scan", PlanStepStatus::InProgress),
                step("verify", PlanStepStatus::Pending),
            ],
        };
        validate_plan(&valid).expect("one active step may lead pending work");

        for (label, params, expected) in [
            (
                "blank title",
                PlanUpdateParams {
                    title: " \t".to_owned(),
                    ..valid.clone()
                },
                "plan title must not be empty",
            ),
            (
                "duplicate ids",
                PlanUpdateParams {
                    steps: vec![
                        step("scan", PlanStepStatus::InProgress),
                        step("scan", PlanStepStatus::Pending),
                    ],
                    ..valid.clone()
                },
                "duplicate plan step id `scan`",
            ),
            (
                "two active steps",
                PlanUpdateParams {
                    steps: vec![
                        step("scan", PlanStepStatus::InProgress),
                        step("verify", PlanStepStatus::InProgress),
                    ],
                    ..valid.clone()
                },
                "at most one plan step may be in_progress",
            ),
            (
                "pending without active work",
                PlanUpdateParams {
                    steps: vec![
                        step("scan", PlanStepStatus::Completed),
                        step("verify", PlanStepStatus::Pending),
                    ],
                    ..valid.clone()
                },
                "pending plan steps require exactly one in_progress step",
            ),
        ] {
            assert!(
                matches!(
                    validate_plan(&params),
                    Err(WorkStateError::Invalid(message)) if message.contains(expected)
                ),
                "{label} was accepted"
            );
        }

        validate_plan(&PlanUpdateParams {
            steps: vec![
                step("scan", PlanStepStatus::Completed),
                step("verify", PlanStepStatus::Completed),
            ],
            ..valid
        })
        .expect("a fully completed plan needs no in_progress step");
    }

    #[test]
    fn plan_goal_ids_are_validated_at_the_utf8_byte_boundary() {
        let valid_goal_id = format!("{}ab", "界".repeat(170));
        assert_eq!(valid_goal_id.len(), 512);
        validate_plan(&PlanUpdateParams {
            expected_revision: None,
            goal_id: Some(valid_goal_id),
            title: "release".to_owned(),
            steps: vec![step("scan", PlanStepStatus::InProgress)],
        })
        .expect("a 512-byte UTF-8 goal identifier remains valid");

        let oversized_goal_id = "界".repeat(171);
        assert_eq!(oversized_goal_id.len(), 513);
        assert!(matches!(
            validate_plan(&PlanUpdateParams {
                expected_revision: None,
                goal_id: Some(oversized_goal_id),
                title: "release".to_owned(),
                steps: vec![step("scan", PlanStepStatus::InProgress)],
            }),
            Err(WorkStateError::Invalid(message))
                if message.contains("plan goal_id")
                    && message.contains("512 bytes")
                    && message.contains("513 bytes")
        ));

        for invalid in [
            "",
            " goal",
            "goal\nid",
            "zuno.invalid-id/v1;field=plan.goal_id;error=forged",
        ] {
            assert!(matches!(
                validate_plan(&PlanUpdateParams {
                    expected_revision: None,
                    goal_id: Some(invalid.to_owned()),
                    title: "release".to_owned(),
                    steps: vec![step("scan", PlanStepStatus::InProgress)],
                }),
                Err(WorkStateError::Invalid(message))
                    if message.contains("plan goal_id")
            ));
        }
    }

    #[test]
    fn plan_updates_reject_a_twenty_kib_goal_id_without_changing_durable_state() {
        let store = store();
        let oversized_goal_id = "g".repeat(20 * 1024);

        let result = store.update_plan(
            "ses",
            PlanUpdateParams {
                expected_revision: None,
                goal_id: Some(oversized_goal_id),
                title: "release".to_owned(),
                steps: vec![step("scan", PlanStepStatus::InProgress)],
            },
        );

        assert!(matches!(
            result,
            Err(WorkStateError::Invalid(message))
                if message.contains("plan goal_id")
                    && message.contains("512 bytes")
                    && message.contains("20480 bytes")
        ));
        assert!(
            store.plan("ses").expect("read plan").is_none(),
            "a rejected identifier must not leave a durable plan"
        );
    }

    #[test]
    fn snapshots_fail_safe_an_existing_oversized_goal_id_without_losing_identity() {
        let store = store();
        let oversized_goal_id = "界".repeat(7_000);
        let mut different_goal_id = oversized_goal_id.clone();
        different_goal_id.push('!');
        let connection = store.pool.get().expect("open connection");
        connection
            .execute(
                "INSERT INTO work_plan \
                 (session_id,id,goal_id,revision,title,steps,time_created,time_updated) \
                 VALUES ('ses','plan_corrupt',?1,1,'release','[]',1,1)",
                [&oversized_goal_id],
            )
            .expect("seed legacy invalid plan");

        let projected = store
            .snapshot("ses")
            .expect("project invalid durable plan")
            .plan
            .expect("plan")
            .goal_id
            .expect("diagnostic goal identity");
        assert!(projected.starts_with("zuno.invalid-id/v1;field=plan.goal_id;"));
        assert!(projected.contains("error=too_long"));
        assert!(projected.contains("value=omitted"));
        assert!(projected.contains(&format!("bytes={}", oversized_goal_id.len())));
        assert!(projected.contains("sha256="));
        assert!(projected.len() <= 512);
        assert_eq!(
            projected,
            store
                .snapshot("ses")
                .expect("repeat projection")
                .plan
                .expect("plan")
                .goal_id
                .expect("diagnostic goal identity"),
            "the diagnostic identity must be deterministic"
        );
        connection
            .execute(
                "UPDATE work_plan SET goal_id=?1 WHERE session_id='ses'",
                [&different_goal_id],
            )
            .expect("replace invalid identity");
        assert_ne!(
            projected,
            store
                .snapshot("ses")
                .expect("project different invalid durable plan")
                .plan
                .expect("plan")
                .goal_id
                .expect("diagnostic goal identity"),
            "different omitted identifiers must retain distinct authority keys"
        );
        connection
            .execute(
                "UPDATE work_plan SET goal_id=?1 WHERE session_id='ses'",
                [&oversized_goal_id],
            )
            .expect("restore invalid identity");

        assert_eq!(
            store
                .plan("ses")
                .expect("read raw durable plan")
                .expect("plan")
                .goal_id
                .as_deref(),
            Some(oversized_goal_id.as_str()),
            "fail-safe projection must not rewrite or masquerade as the durable value"
        );
    }

    #[test]
    fn plan_updates_keep_step_ids_and_completed_steps_stable() {
        let store = store();
        let first = store
            .update_plan(
                "ses",
                PlanUpdateParams {
                    expected_revision: None,
                    goal_id: None,
                    title: "release".to_owned(),
                    steps: vec![
                        step("scan", PlanStepStatus::Completed),
                        step("verify", PlanStepStatus::InProgress),
                    ],
                },
            )
            .expect("create plan");

        let removed = store.update_plan(
            "ses",
            PlanUpdateParams {
                expected_revision: Some(first.revision),
                goal_id: None,
                title: "release".to_owned(),
                steps: vec![step("verify", PlanStepStatus::InProgress)],
            },
        );
        assert!(matches!(
            removed,
            Err(WorkStateError::Invalid(message))
                if message.contains("existing plan step id `scan` must remain stable")
        ));
        assert_eq!(
            store
                .plan("ses")
                .expect("read plan")
                .expect("plan")
                .revision,
            first.revision,
            "a rejected update changed durable state"
        );

        let regressed = store.update_plan(
            "ses",
            PlanUpdateParams {
                expected_revision: Some(first.revision),
                goal_id: None,
                title: "release".to_owned(),
                steps: vec![
                    step("scan", PlanStepStatus::Pending),
                    step("verify", PlanStepStatus::InProgress),
                ],
            },
        );
        assert!(matches!(
            regressed,
            Err(WorkStateError::Invalid(message))
                if message.contains(
                    "terminal plan step `scan` cannot change from `completed` to `pending`"
                )
        ));

        let completed = store
            .update_plan(
                "ses",
                PlanUpdateParams {
                    expected_revision: Some(first.revision),
                    goal_id: None,
                    title: "release".to_owned(),
                    steps: vec![
                        step("scan", PlanStepStatus::Completed),
                        step("verify", PlanStepStatus::Completed),
                    ],
                },
            )
            .expect("complete plan");
        assert_eq!(completed.revision, first.revision + 1);
    }

    #[test]
    fn plan_patch_changes_only_named_steps_without_retransmitting_integrate() {
        let store = store();
        let created = store
            .mutate_plan(
                "ses",
                PlanMutationParams::Create {
                    expected_revision: None,
                    goal_id: None,
                    title: "Ship the change".to_owned(),
                    steps: vec![
                        PlanStepInput {
                            title: "Execute".to_owned(),
                            status: PlanStepStatus::InProgress,
                        },
                        PlanStepInput {
                            title: "Integrate".to_owned(),
                            status: PlanStepStatus::Pending,
                        },
                    ],
                },
            )
            .expect("create operation");
        let execute_id = created.steps[0].id.clone();
        let integrate = created.steps[1].clone();

        let patched = store
            .mutate_plan(
                "ses",
                PlanMutationParams::Patch {
                    expected_revision: created.revision,
                    title: None,
                    steps: vec![PlanStepPatch {
                        id: execute_id.clone(),
                        title: None,
                        status: Some(PlanStepStatus::Completed),
                    }],
                },
            )
            .expect_err("pending integrate still requires an active step");
        assert!(matches!(
            patched,
            WorkStateError::Invalid(message)
                if message.contains("pending plan steps require exactly one in_progress step")
        ));

        let patched = store
            .mutate_plan(
                "ses",
                PlanMutationParams::Patch {
                    expected_revision: created.revision,
                    title: None,
                    steps: vec![
                        PlanStepPatch {
                            id: execute_id,
                            title: None,
                            status: Some(PlanStepStatus::Completed),
                        },
                        PlanStepPatch {
                            id: integrate.id.clone(),
                            title: None,
                            status: Some(PlanStepStatus::InProgress),
                        },
                    ],
                },
            )
            .expect("patch only changed steps");
        assert_eq!(patched.steps[1].id, integrate.id);
        assert_eq!(patched.steps[1].title, integrate.title);
        assert_eq!(patched.steps[1].status, PlanStepStatus::InProgress);
    }

    #[test]
    fn plan_append_assigns_host_ids_and_pop_requires_only_the_revision() {
        let store = store();
        let parent = store
            .mutate_plan(
                "ses",
                PlanMutationParams::Create {
                    expected_revision: None,
                    goal_id: Some("goal".to_owned()),
                    title: "Parent".to_owned(),
                    steps: vec![PlanStepInput {
                        title: "Parent work".to_owned(),
                        status: PlanStepStatus::InProgress,
                    }],
                },
            )
            .expect("create parent");
        let appended = store
            .mutate_plan(
                "ses",
                PlanMutationParams::Append {
                    expected_revision: parent.revision,
                    steps: vec![PlanStepInput {
                        title: "Verify".to_owned(),
                        status: PlanStepStatus::Pending,
                    }],
                },
            )
            .expect("append step");
        assert!(appended.steps[1].id.starts_with("step_"));
        assert_ne!(appended.steps[0].id, appended.steps[1].id);

        let child = store
            .mutate_plan(
                "ses",
                PlanMutationParams::Push {
                    expected_revision: appended.revision,
                    title: "Focused repair".to_owned(),
                    steps: vec![PlanStepInput {
                        title: "Repair".to_owned(),
                        status: PlanStepStatus::InProgress,
                    }],
                },
            )
            .expect("push child");
        let child = store
            .mutate_plan(
                "ses",
                PlanMutationParams::Patch {
                    expected_revision: child.revision,
                    title: None,
                    steps: vec![PlanStepPatch {
                        id: child.steps[0].id.clone(),
                        title: None,
                        status: Some(PlanStepStatus::Completed),
                    }],
                },
            )
            .expect("complete child");
        let restored = store
            .mutate_plan(
                "ses",
                PlanMutationParams::Pop {
                    expected_revision: child.revision,
                },
            )
            .expect("pop using revision only");
        assert_eq!(restored, appended);
    }

    #[test]
    fn plan_steps_cannot_complete_before_linked_jobs_are_reconciled() {
        let store = store();
        let first = store
            .update_plan(
                "ses",
                PlanUpdateParams {
                    expected_revision: None,
                    goal_id: None,
                    title: "release".to_owned(),
                    steps: vec![step("verify", PlanStepStatus::InProgress)],
                },
            )
            .expect("create plan");
        let jobs = zuno_db::job::AgentJobStore::new(Arc::clone(&store.pool));
        jobs.create(
            zuno_db::job::NewAgentJob::new(
                "job_verify",
                "ses",
                zuno_db::job::JobSubject::child_session("ses_child"),
                zuno_db::job::ReportDelivery::Quiet,
                10,
            )
            .with_work_context(Some(zuno_db::job::JobWorkContext::new(
                None,
                first.id.clone(),
                first.revision,
                "verify",
            ))),
        )
        .expect("create linked job");

        let blocked = store.update_plan(
            "ses",
            PlanUpdateParams {
                expected_revision: Some(first.revision),
                goal_id: None,
                title: "release".to_owned(),
                steps: vec![step("verify", PlanStepStatus::Completed)],
            },
        );
        assert!(matches!(
            blocked,
            Err(WorkStateError::Invalid(message))
                if message.contains("linked job `job_verify` is `running`")
        ));

        jobs.settle(
            "job_verify",
            zuno_db::job::JobSettlement::failed("verification failed", 20, None),
        )
        .expect("settle linked job");
        let completed = store
            .update_plan(
                "ses",
                PlanUpdateParams {
                    expected_revision: Some(first.revision),
                    goal_id: None,
                    title: "release".to_owned(),
                    steps: vec![step("verify", PlanStepStatus::Completed)],
                },
            )
            .expect("complete reconciled plan step");
        assert_eq!(completed.revision, first.revision + 1);
    }

    #[test]
    fn plan_updates_require_the_current_revision() {
        let store = store();
        let first = store
            .update_plan(
                "ses",
                PlanUpdateParams {
                    expected_revision: None,
                    goal_id: Some("goal".to_owned()),
                    title: "release".to_owned(),
                    steps: vec![step("scan", PlanStepStatus::InProgress)],
                },
            )
            .expect("create plan");
        assert_eq!(first.revision, 1);
        let second = store
            .update_plan(
                "ses",
                PlanUpdateParams {
                    expected_revision: Some(1),
                    goal_id: first.goal_id.clone(),
                    title: "release safely".to_owned(),
                    steps: vec![
                        step("scan", PlanStepStatus::Completed),
                        step("verify", PlanStepStatus::InProgress),
                    ],
                },
            )
            .expect("update plan");
        assert_eq!(second.revision, 2);
        assert!(matches!(
            store.update_plan(
                "ses",
                PlanUpdateParams {
                    expected_revision: Some(1),
                    goal_id: None,
                    title: "stale".to_owned(),
                    steps: vec![],
                }
            ),
            Err(WorkStateError::RevisionConflict { .. })
        ));
    }

    #[test]
    fn focused_subplan_survives_restart_and_restores_its_parent_once() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let location = zuno_paths::DbLocation::File(directory.path().join("zuno.db"));
        let store = initialize_store(&location);
        let parent = store
            .update_plan(
                "ses",
                PlanUpdateParams {
                    expected_revision: None,
                    goal_id: Some("goal".to_owned()),
                    title: "Release Zuno".to_owned(),
                    steps: vec![
                        step("implement", PlanStepStatus::InProgress),
                        step("publish", PlanStepStatus::Pending),
                    ],
                },
            )
            .expect("create parent plan");
        let child = store
            .push_plan(
                "ses",
                PlanUpdateParams {
                    expected_revision: Some(parent.revision),
                    goal_id: parent.goal_id.clone(),
                    title: "Repair Windows tests".to_owned(),
                    steps: vec![
                        step("diagnose", PlanStepStatus::InProgress),
                        step("verify", PlanStepStatus::Pending),
                    ],
                },
            )
            .expect("push child plan");
        assert_eq!(child.parent_plan_id.as_deref(), Some(parent.id.as_str()));
        assert_eq!(child.stack_depth, 1);
        drop(store);

        let reopened = WorkStateStore::new(Arc::new(
            Pool::open(&location).expect("reopen work-state database"),
        ));
        assert_eq!(
            reopened.plan("ses").expect("read child after restart"),
            Some(child.clone()),
            "restart must not flatten or reconstruct the active child from prose"
        );
        let restored = reopened
            .complete_subplan(
                "ses",
                PlanUpdateParams {
                    expected_revision: Some(child.revision),
                    goal_id: child.goal_id.clone(),
                    title: child.title.clone(),
                    steps: vec![
                        step("diagnose", PlanStepStatus::Completed),
                        step("verify", PlanStepStatus::Completed),
                    ],
                },
            )
            .expect("complete child and restore parent");
        assert_eq!(restored, parent);
        assert!(matches!(
            reopened.complete_subplan(
                "ses",
                PlanUpdateParams {
                    expected_revision: Some(restored.revision),
                    goal_id: restored.goal_id.clone(),
                    title: restored.title.clone(),
                    steps: vec![
                        step("implement", PlanStepStatus::Completed),
                        step("publish", PlanStepStatus::Completed),
                    ],
                }
            ),
            Err(WorkStateError::Invalid(message))
                if message.contains("no suspended parent")
        ));
        let connection = reopened.pool.get().expect("open archive connection");
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM work_plan_archive \
                     WHERE session_id='ses' AND state='completed'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("count completed child"),
            1
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM work_plan_archive \
                     WHERE session_id='ses' AND state='suspended'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("count suspended plans"),
            0
        );
    }

    #[test]
    fn subplan_pop_requires_every_child_step_to_be_completed() {
        let store = store();
        let parent = store
            .update_plan(
                "ses",
                PlanUpdateParams {
                    expected_revision: None,
                    goal_id: None,
                    title: "Parent".to_owned(),
                    steps: vec![step("parent", PlanStepStatus::InProgress)],
                },
            )
            .expect("create parent");
        let child = store
            .push_plan(
                "ses",
                PlanUpdateParams {
                    expected_revision: Some(parent.revision),
                    goal_id: None,
                    title: "Child".to_owned(),
                    steps: vec![step("child", PlanStepStatus::InProgress)],
                },
            )
            .expect("push child");

        assert!(matches!(
            store.complete_subplan(
                "ses",
                PlanUpdateParams {
                    expected_revision: Some(child.revision),
                    goal_id: None,
                    title: child.title.clone(),
                    steps: child.steps.clone(),
                }
            ),
            Err(WorkStateError::Invalid(message))
                if message.contains("only after every child step is completed")
        ));
        assert_eq!(store.plan("ses").expect("read active child"), Some(child));
    }

    #[test]
    fn replacing_an_objective_supersedes_the_whole_suspended_stack() {
        let store = store();
        let parent = store
            .update_plan(
                "ses",
                PlanUpdateParams {
                    expected_revision: None,
                    goal_id: Some("goal-old".to_owned()),
                    title: "Old objective".to_owned(),
                    steps: vec![step("old", PlanStepStatus::InProgress)],
                },
            )
            .expect("create parent");
        let child = store
            .push_plan(
                "ses",
                PlanUpdateParams {
                    expected_revision: Some(parent.revision),
                    goal_id: parent.goal_id.clone(),
                    title: "Temporary investigation".to_owned(),
                    steps: vec![step("investigate", PlanStepStatus::InProgress)],
                },
            )
            .expect("push child");

        let replacement = store
            .replace_plan_for_objective(
                "ses",
                PlanUpdateParams {
                    expected_revision: Some(child.revision),
                    goal_id: Some("goal-new".to_owned()),
                    title: "New objective".to_owned(),
                    steps: vec![step("scope", PlanStepStatus::InProgress)],
                },
            )
            .expect("replace objective");
        assert_ne!(replacement.id, child.id);
        assert_eq!(replacement.parent_plan_id, None);
        assert_eq!(replacement.stack_depth, 0);
        assert_eq!(replacement.steps.len(), 1);
        let connection = store.pool.get().expect("open archive connection");
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM work_plan_archive \
                     WHERE session_id='ses' AND state='superseded'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("count superseded frames"),
            2,
            "both the active child and its suspended parent must be terminalized"
        );
    }

    #[test]
    fn work_items_keep_ids_revisions_and_validate_dependencies_atomically() {
        let store = store();
        let added = store
            .update_items(
                "ses",
                vec![WorkItemChange::Add {
                    id: Some("todo_a".to_owned()),
                    goal_id: None,
                    plan_step_id: None,
                    parent_id: None,
                    subject: "scan".to_owned(),
                    description: "scan repository".to_owned(),
                    active_form: Some("Scanning".to_owned()),
                    status: WorkItemStatus::InProgress,
                    priority: WorkItemPriority::High,
                    dependencies: vec![],
                    owner: Some("researcher".to_owned()),
                }],
            )
            .expect("add work item");
        assert_eq!(added[0].revision, 1);
        let updated = store
            .update_items(
                "ses",
                vec![WorkItemChange::Update {
                    id: "todo_a".to_owned(),
                    expected_revision: 1,
                    goal_id: None,
                    plan_step_id: None,
                    parent_id: None,
                    subject: "scan".to_owned(),
                    description: "scan complete".to_owned(),
                    active_form: None,
                    status: WorkItemStatus::Completed,
                    priority: WorkItemPriority::High,
                    dependencies: vec![],
                    owner: Some("researcher".to_owned()),
                }],
            )
            .expect("update work item");
        assert_eq!(updated[0].revision, 2);
        let invalid = store.update_items(
            "ses",
            vec![WorkItemChange::Add {
                id: Some("todo_b".to_owned()),
                goal_id: None,
                plan_step_id: None,
                parent_id: None,
                subject: "verify".to_owned(),
                description: "verify".to_owned(),
                active_form: None,
                status: WorkItemStatus::Pending,
                priority: WorkItemPriority::Medium,
                dependencies: vec!["missing".to_owned()],
                owner: None,
            }],
        );
        assert!(matches!(invalid, Err(WorkStateError::Invalid(_))));
        assert_eq!(store.items("ses").expect("list items").len(), 1);
    }

    #[test]
    fn todo_update_description_explains_same_batch_dependency_ids() {
        assert!(
            TODO_UPDATE_DESCRIPTION.contains(
                "assign explicit stable ids to every newly added item before referencing them"
            ),
            "the model must not invent positional ids for dependencies inside one atomic batch"
        );
    }

    #[test]
    fn plan_update_description_explains_the_active_step_invariant() {
        assert!(
            PLAN_UPDATE_DESCRIPTION.contains("Pending steps require exactly one in_progress step"),
            "the model must know the durable plan's active-step invariant before calling the tool"
        );
        assert!(
            PLAN_UPDATE_DESCRIPTION.contains("append a new artifact-scoped verification step"),
            "changed artifacts require a new immutable verification gate"
        );
        assert!(
            PLAN_UPDATE_DESCRIPTION
                .contains("Do not complete or supersede a step while a linked Job"),
            "the model must reconcile durable child work before closing its Plan step"
        );
        assert!(
            PLAN_UPDATE_DESCRIPTION.contains("action=push")
                && PLAN_UPDATE_DESCRIPTION.contains("action=pop")
                && PLAN_UPDATE_DESCRIPTION.contains("host generates stable step ids")
                && PLAN_UPDATE_DESCRIPTION.contains("Before a final answer"),
            "the model must know how focused subplans restore their parent and when to reconcile"
        );
    }

    #[test]
    fn parallel_runtime_items_are_metered_without_model_owned_usage_fields() {
        let store = store();
        store
            .update_items(
                "ses",
                ["scan", "review"]
                    .into_iter()
                    .map(|id| WorkItemChange::Add {
                        id: Some(format!("todo_{id}")),
                        goal_id: None,
                        plan_step_id: None,
                        parent_id: None,
                        subject: id.to_owned(),
                        description: format!("run {id}"),
                        active_form: Some(format!("Running {id}")),
                        status: WorkItemStatus::Pending,
                        priority: WorkItemPriority::Medium,
                        dependencies: Vec::new(),
                        owner: Some("workflow".to_owned()),
                    })
                    .collect(),
            )
            .expect("admit parallel work");

        let scan = store
            .transition_runtime_item(
                "ses",
                "todo_scan",
                1,
                WorkItemStatus::InProgress,
                None,
                None,
            )
            .expect("start scan");
        let review = store
            .transition_runtime_item(
                "ses",
                "todo_review",
                1,
                WorkItemStatus::InProgress,
                None,
                None,
            )
            .expect("start review concurrently");
        assert_eq!((scan.revision, review.revision), (2, 2));

        let scan = store
            .transition_runtime_item(
                "ses",
                "todo_scan",
                2,
                WorkItemStatus::Completed,
                Some(25),
                Some(42),
            )
            .expect("complete scan");
        assert_eq!(scan.revision, 3);
        assert_eq!(scan.tokens_used, 42);
        assert!(scan.usage_known);
        assert_eq!(scan.time_used_ms, 25);
        assert!(scan.active_form.is_none());

        let review = store
            .transition_runtime_item(
                "ses",
                "todo_review",
                2,
                WorkItemStatus::Cancelled,
                Some(10),
                None,
            )
            .expect("cancel review");
        assert!(!review.usage_known);
        assert_eq!(review.tokens_used, 0);
        assert!(matches!(
            store.transition_runtime_item(
                "ses",
                "todo_scan",
                2,
                WorkItemStatus::Blocked,
                Some(30),
                None,
            ),
            Err(WorkStateError::RevisionConflict { .. })
        ));
    }

    #[test]
    fn cyclic_work_item_batch_rolls_back_atomically() {
        let store = store();
        let invalid = store.update_items(
            "ses",
            vec![
                WorkItemChange::Add {
                    id: Some("todo_a".to_owned()),
                    goal_id: None,
                    plan_step_id: None,
                    parent_id: None,
                    subject: "scan".to_owned(),
                    description: "scan repository".to_owned(),
                    active_form: None,
                    status: WorkItemStatus::Pending,
                    priority: WorkItemPriority::High,
                    dependencies: vec!["todo_b".to_owned()],
                    owner: Some("researcher".to_owned()),
                },
                WorkItemChange::Add {
                    id: Some("todo_b".to_owned()),
                    goal_id: None,
                    plan_step_id: None,
                    parent_id: None,
                    subject: "review".to_owned(),
                    description: "review scan".to_owned(),
                    active_form: None,
                    status: WorkItemStatus::Pending,
                    priority: WorkItemPriority::Medium,
                    dependencies: vec!["todo_a".to_owned()],
                    owner: Some("reviewer".to_owned()),
                },
            ],
        );
        assert!(
            matches!(invalid, Err(WorkStateError::Invalid(message)) if message.contains("cycle"))
        );
        assert!(
            store.items("ses").expect("list items").is_empty(),
            "a rejected graph must not leave either row behind"
        );
    }
}

//! Trusted native Goal-turn binding and durable, once-only settlement.
//!
//! A cycle can contain many engine turns. Bind each actual engine turn before
//! dispatch, then settle each completed engine turn before binding its successor.
//! No timestamp ordering or model-provided identity is used.

use super::*;
use crate::GoalTurnOutcome;
use crate::error::GoalTurnConflictReason;
use zuno_tool::ToolContext;

/// Copy this DDL into the parent-owned database migration; zuno-db cannot import
/// zuno-goal without a dependency cycle. No legacy Goal tables are altered.
pub const GOAL_TURN_SCHEMA: &str = include_str!("goal_turn_schema.sql");

/// Host-issued identity. Session identity is supplied separately to every operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GoalTurnIdentity {
    pub goal_id: String,
    pub cycle_id: String,
    /// Actual engine turn, not a logical request, provider attempt, or tool call.
    pub turn_id: String,
}

impl GoalTurnIdentity {
    pub fn new(
        goal_id: impl Into<String>,
        cycle_id: impl Into<String>,
        turn_id: impl Into<String>,
    ) -> Result<Self, GoalError> {
        let identity = Self {
            goal_id: goal_id.into(),
            cycle_id: cycle_id.into(),
            turn_id: turn_id.into(),
        };
        identity.validate()?;
        Ok(identity)
    }

    fn validate(&self) -> Result<(), GoalError> {
        for (field, value) in [
            ("goal_id", &self.goal_id),
            ("cycle_id", &self.cycle_id),
            ("turn_id", &self.turn_id),
        ] {
            if value.trim().is_empty() || value.len() > 512 {
                return Err(GoalError::InvalidGoalTurnIdentity { field });
            }
        }
        Ok(())
    }
}

/// One model observation; identity comes from native context, not tool JSON.
#[derive(Debug, Clone, Copy)]
pub struct GoalTurnObservationUpdate<'a> {
    pub identity: &'a GoalTurnIdentity,
    pub expected_revision: i64,
    pub signal: &'a str,
    pub satisfy: &'a [CriterionSatisfaction],
    pub waive: &'a [CriterionWaiver],
}

/// Accepted staging snapshot. The reported count excludes this unfinished turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalTurnObservationReceipt {
    pub identity: GoalTurnIdentity,
    pub goal: Goal,
    pub criteria: Vec<GoalCriterion>,
    pub completed_turn_streak: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalTurnDisposition {
    Reset,
    Pending,
    Blocked,
    /// The same Goal already paused or finished; this turn changes no Goal state.
    Inactive,
}

/// Immutable historical result of one completed real turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GoalTurnAudit {
    pub session_id: String,
    pub identity: GoalTurnIdentity,
    pub disposition: GoalTurnDisposition,
    pub status: GoalStatus,
    pub goal_revision: i64,
    pub signal: Option<String>,
    pub completed_turn_streak: u32,
}

/// Separates the historical result from the Goal currently stored on replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalTurnSettlement {
    pub audit: GoalTurnAudit,
    pub replayed: bool,
    pub current_goal: Option<Goal>,
}

#[derive(Debug)]
struct CycleFailure {
    identity: GoalTurnIdentity,
    signal: Option<String>,
    consecutive_turns: u32,
}

impl GoalStore {
    /// Production `goal_propose`: validate the immutable Attempt against the
    /// native current cycle/fence, then create and bind the Goal atomically.
    /// An unfinished existing Goal can neither be replaced nor resumed here.
    pub fn create_goal_from_tool(
        &self,
        context: &ToolContext,
        objective: &str,
        success_criteria: &[String],
        token_budget: Option<i64>,
    ) -> Result<GoalCreation, GoalError> {
        let criteria = normalize_model_success_criteria(success_criteria)?;
        let stamp_ms = now_ms()?;
        self.pool.try_transaction(|tx| {
            let mut scope = tool_scope_in(tx, context)?;
            let current = Self::goal_in(tx, &context.session_id)?;
            if let Some(current) = &current
                && !matches!(current.status, GoalStatus::Complete | GoalStatus::Cancelled)
            {
                return Err(GoalError::GoalNotReplaceable {
                    session_id: context.session_id.clone(), status: current.status,
                });
            }
            if scope.goal_id.is_some()
                && scope.goal_id.as_deref() != current.as_ref().map(|goal| goal.goal_id.as_str())
            {
                return Err(turn_conflict(&context.session_id, GoalTurnConflictReason::GoalUnowned));
            }
            // Do not spill files for rejected/stale authority or an unfinished Goal.
            let objective = spill::store_objective(&self.spill_dir, objective)?;
            let created = create_goal_in(
                tx, &context.session_id, &new_goal_id(), &objective, &criteria, token_budget, stamp_ms,
            )?;
            scope.goal_id = Some(created.goal.goal_id.clone());
            zuno_db::session_work_cycle::save_in(tx, &scope, stamp_ms)?;
            let identity = GoalTurnIdentity::new(
                &created.goal.goal_id, &scope.cycle_id,
                scope.active_turn_id.as_deref().ok_or(GoalError::GoalTurnContextMissing)?,
            )?;
            // Creation discarded the predecessor's cursor inside this transaction.
            Self::bind_goal_turn_in(tx, &context.session_id, &identity, None)?;
            zuno_db::event_log::append_in(tx, &context.session_id,
                zuno_db::event_log::NewSessionEvent::new(
                    "session.goal.created_and_bound",
                    serde_json::json!({
                        "identity": identity, "goalRevision": created.goal.revision, "time": stamp_ms
                    }).as_object().expect("object").clone(),
                )?,
            )?;
            Ok(created)
        })
    }

    /// Production model completion requires the current Goal/cycle/real turn,
    /// not merely a revision supplied in tool JSON. Authority checks and all
    /// criterion/status/history changes share the same transaction.
    pub fn complete_goal_from_tool(
        &self,
        context: &ToolContext,
        expected_revision: i64,
        satisfy: &[CriterionSatisfaction],
        waive: &[CriterionWaiver],
    ) -> Result<Option<Goal>, GoalError> {
        let stamp_ms = now_ms()?;
        self.pool.try_transaction(|tx| {
            let scope = tool_scope_in(tx, context)?;
            if scope.goal_id.is_none() {
                return Err(turn_conflict(
                    &context.session_id,
                    GoalTurnConflictReason::GoalUnowned,
                ));
            }
            let identity = Self::current_goal_turn_in(tx, &context.session_id)?
                .ok_or(GoalError::GoalTurnContextMissing)?;
            if scope.cycle_id != identity.cycle_id
                || scope.active_turn_id.as_deref() != Some(identity.turn_id.as_str())
            {
                return Err(turn_conflict(
                    &context.session_id,
                    GoalTurnConflictReason::AttemptTurnMismatch,
                ));
            }
            matching_goal(tx, &context.session_id, &identity)?;
            refuse_settled(tx, &context.session_id, &identity)?;
            complete_as_model_with_criteria_in(
                tx,
                &context.session_id,
                expected_revision,
                satisfy,
                waive,
                stamp_ms,
            )
        })
    }

    /// Read the native cursor before a compare-and-swap bind.
    pub fn current_goal_turn(
        &self,
        session_id: &str,
    ) -> Result<Option<GoalTurnIdentity>, GoalError> {
        let connection = self.pool.get()?;
        Self::current_goal_turn_in(&connection, session_id)
    }

    pub fn current_goal_turn_in(
        connection: &Connection,
        session_id: &str,
    ) -> Result<Option<GoalTurnIdentity>, GoalError> {
        Ok(cycle_failure_in(connection, session_id)?.map(|row| row.identity))
    }

    /// Native-only admission. The host must already own the session's execution
    /// permit and authorize this Goal/cycle. Allocate a fresh engine turn ID for a
    /// new execution; never reuse an older superseded ID. A stale binder cannot replace a newer
    /// cursor. Rebinding the same uncompleted tuple does not reset its streak.
    pub fn bind_goal_turn_checked(
        &self,
        session_id: &str,
        identity: &GoalTurnIdentity,
        expected_previous: Option<&GoalTurnIdentity>,
    ) -> Result<(), GoalError> {
        self.pool.try_transaction(|tx| {
            Self::bind_goal_turn_in(tx, session_id, identity, expected_previous)
        })
    }

    /// Combine with the host's execution-state update in one transaction.
    pub fn bind_goal_turn_in(
        tx: &Transaction<'_>,
        session_id: &str,
        identity: &GoalTurnIdentity,
        expected_previous: Option<&GoalTurnIdentity>,
    ) -> Result<(), GoalError> {
        identity.validate()?;
        let goal = matching_goal(tx, session_id, identity)?;
        if goal.status != GoalStatus::Active {
            return Err(GoalError::GoalNotActive {
                session_id: session_id.to_owned(),
                status: goal.status,
            });
        }
        refuse_settled(tx, session_id, identity)?;
        let previous = cycle_failure_in(tx, session_id)?;
        if previous.as_ref().map(|row| &row.identity) == Some(identity) {
            return Ok(());
        }
        if previous.as_ref().map(|row| &row.identity) != expected_previous {
            return Err(turn_conflict(
                session_id,
                GoalTurnConflictReason::NativeBindingChanged,
            ));
        }
        let same_cycle = previous.as_ref().filter(|row| {
            row.identity.goal_id == identity.goal_id && row.identity.cycle_id == identity.cycle_id
        });
        tx.execute(
            "INSERT INTO goal_cycle_failure \
             (session_id,goal_id,cycle_id,active_turn_id,signal,consecutive_turns) \
             VALUES (?1,?2,?3,?4,?5,?6) \
             ON CONFLICT(session_id) DO UPDATE SET goal_id=excluded.goal_id, \
                 cycle_id=excluded.cycle_id,active_turn_id=excluded.active_turn_id, \
                 signal=excluded.signal,consecutive_turns=excluded.consecutive_turns",
            params![
                session_id,
                identity.goal_id,
                identity.cycle_id,
                identity.turn_id,
                same_cycle.and_then(|row| row.signal.as_deref()),
                same_cycle.map_or(0, |row| row.consecutive_turns),
            ],
        )
        .map_err(zuno_db::map_error)?;
        Ok(())
    }

    /// Resolve the existing immutable AttemptSnapshot against native binding.
    /// There is deliberately no fallback to a current Goal or model-supplied IDs.
    /// The staging transaction checks the returned identity again.
    pub fn goal_turn_for_tool(&self, context: &ToolContext) -> Result<GoalTurnIdentity, GoalError> {
        let snapshot = context
            .orchestration_snapshot()
            .ok_or(GoalError::GoalTurnContextMissing)?;
        if snapshot.owner.session_id != context.session_id {
            return Err(turn_conflict(
                &context.session_id,
                GoalTurnConflictReason::AttemptSessionMismatch,
            ));
        }
        let identity = self
            .current_goal_turn(&context.session_id)?
            .ok_or(GoalError::GoalTurnContextMissing)?;
        if snapshot.turn_id != identity.turn_id
            || snapshot.cycle_id.as_deref() != Some(identity.cycle_id.as_str())
        {
            return Err(turn_conflict(
                &context.session_id,
                GoalTurnConflictReason::AttemptTurnMismatch,
            ));
        }
        Ok(identity)
    }

    pub fn stage_goal_turn_observation(
        &self,
        session_id: &str,
        update: GoalTurnObservationUpdate<'_>,
    ) -> Result<GoalTurnObservationReceipt, GoalError> {
        let stamp_ms = now_ms()?;
        self.pool.try_transaction(|tx| {
            Self::stage_goal_turn_observation_in(tx, session_id, update, stamp_ms)
        })
    }

    /// Identity/revision checks, criterion changes and staging are all-or-nothing.
    pub fn stage_goal_turn_observation_in(
        tx: &Transaction<'_>,
        session_id: &str,
        update: GoalTurnObservationUpdate<'_>,
        stamp_ms: i64,
    ) -> Result<GoalTurnObservationReceipt, GoalError> {
        let identity = update.identity;
        identity.validate()?;
        let signal = update.signal.trim();
        if signal.is_empty() {
            return Err(turn_conflict(
                session_id,
                GoalTurnConflictReason::EmptySignal,
            ));
        }
        refuse_settled(tx, session_id, identity)?;
        let goal = matching_goal(tx, session_id, identity)?;
        let cursor = matching_cursor(tx, session_id, identity)?;
        if goal.revision != update.expected_revision {
            return Err(GoalError::RevisionConflict {
                session_id: session_id.to_owned(),
                expected: update.expected_revision,
                actual: goal.revision,
            });
        }
        if goal.status != GoalStatus::Active {
            return Err(GoalError::GoalNotActive {
                session_id: session_id.to_owned(),
                status: goal.status,
            });
        }
        for item in update.satisfy {
            satisfy_criterion_in(
                tx,
                &goal,
                session_id,
                item.criterion_id.trim(),
                item.receipt_id.trim(),
                stamp_ms,
            )?;
        }
        for item in update.waive {
            waive_criterion_in(
                tx,
                session_id,
                item.criterion_id.trim(),
                &item.reason,
                stamp_ms,
            )?;
        }
        let goal = if update.satisfy.is_empty() && update.waive.is_empty() {
            goal
        } else {
            touch_goal(tx, session_id, update.expected_revision, stamp_ms)?
        };
        tx.execute(
            "INSERT INTO goal_turn_observation \
             (session_id,goal_id,cycle_id,turn_id,signal,time_created) VALUES (?1,?2,?3,?4,?5,?6) \
             ON CONFLICT(session_id,goal_id,cycle_id,turn_id) DO UPDATE SET signal=excluded.signal",
            params![
                session_id,
                identity.goal_id,
                identity.cycle_id,
                identity.turn_id,
                signal,
                stamp_ms
            ],
        )
        .map_err(zuno_db::map_error)?;
        Ok(GoalTurnObservationReceipt {
            identity: identity.clone(),
            goal,
            criteria: criteria_from(tx, session_id)?,
            completed_turn_streak: if cursor.signal.as_deref() == Some(signal) {
                cursor.consecutive_turns
            } else {
                0
            },
        })
    }

    /// Settle one completed engine turn. Re-delivery returns its immutable receipt,
    /// even after restart, pause or Goal replacement, without touching newer state.
    pub fn settle_goal_turn(
        &self,
        session_id: &str,
        identity: &GoalTurnIdentity,
        outcome: GoalTurnOutcome<'_>,
    ) -> Result<GoalTurnSettlement, GoalError> {
        let stamp_ms = now_ms()?;
        self.pool.try_transaction(|tx| {
            Self::settle_goal_turn_in(tx, session_id, identity, outcome, stamp_ms)
        })
    }

    /// Use inside the host's real-turn terminal-event transaction. Reads no legacy
    /// pending/streak rows. A late uncompleted tuple is rejected before consumption.
    pub fn settle_goal_turn_in(
        tx: &Transaction<'_>,
        session_id: &str,
        identity: &GoalTurnIdentity,
        outcome: GoalTurnOutcome<'_>,
        stamp_ms: i64,
    ) -> Result<GoalTurnSettlement, GoalError> {
        identity.validate()?;
        if let Some(audit) = audit_in(tx, session_id, identity)? {
            return Ok(GoalTurnSettlement {
                audit,
                replayed: true,
                current_goal: Self::goal_in(tx, session_id)?,
            });
        }
        let mut goal = matching_goal(tx, session_id, identity)?;
        // A pause/resume invalidates the cursor. Never let a late old tuple observe
        // the same Goal active again and count against the resumed run.
        let cursor = matching_cursor(tx, session_id, identity)?;
        let staged: Option<String> = tx
            .query_row(
                "DELETE FROM goal_turn_observation WHERE session_id=?1 AND goal_id=?2 \
             AND cycle_id=?3 AND turn_id=?4 RETURNING signal",
                params![
                    session_id,
                    identity.goal_id,
                    identity.cycle_id,
                    identity.turn_id
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(zuno_db::map_error)?;
        let (disposition, signal, count) = if goal.status != GoalStatus::Active {
            (
                GoalTurnDisposition::Inactive,
                cursor.signal,
                cursor.consecutive_turns,
            )
        } else {
            let signal = match outcome {
                GoalTurnOutcome::Progress => staged,
                GoalTurnOutcome::Blocking(signal) => Some(signal.trim().to_owned()),
            }
            .filter(|signal| !signal.trim().is_empty());
            let count = signal.as_ref().map_or(0, |signal| {
                if cursor.signal.as_ref() == Some(signal) {
                    cursor
                        .consecutive_turns
                        .saturating_add(1)
                        .min(crate::BLOCKED_TURN_THRESHOLD)
                } else {
                    1
                }
            });
            tx.execute(
                "UPDATE goal_cycle_failure SET signal=?2,consecutive_turns=?3 WHERE session_id=?1",
                params![session_id, signal, count],
            )
            .map_err(zuno_db::map_error)?;
            clear_retry_state(tx, session_id)?;
            let disposition = if count == crate::BLOCKED_TURN_THRESHOLD {
                let mut statement = tx
                    .prepare(BLOCK_ACTIVE_WITH_REASON)
                    .map_err(zuno_db::map_error)?;
                goal = read_optional(&mut statement, params![signal, stamp_ms, session_id])?
                    .ok_or_else(|| {
                        turn_conflict(session_id, GoalTurnConflictReason::GoalStateConflict)
                    })?;
                tx.execute(
                    "DELETE FROM goal_pause WHERE session_id=?1",
                    params![session_id],
                )
                .map_err(zuno_db::map_error)?;
                GoalTurnDisposition::Blocked
            } else if count == 0 {
                GoalTurnDisposition::Reset
            } else {
                GoalTurnDisposition::Pending
            };
            (disposition, signal, count)
        };
        let audit = GoalTurnAudit {
            session_id: session_id.to_owned(),
            identity: identity.clone(),
            disposition,
            status: goal.status,
            goal_revision: goal.revision,
            signal,
            completed_turn_streak: count,
        };
        let json = serde_json::to_string(&audit).map_err(|source| DbError::Query {
            source: Box::new(source),
        })?;
        tx.execute(
            "INSERT INTO goal_turn_audit (session_id,goal_id,cycle_id,turn_id,audit,time_recorded) \
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                session_id,
                identity.goal_id,
                identity.cycle_id,
                identity.turn_id,
                json,
                stamp_ms
            ],
        )
        .map_err(zuno_db::map_error)?;
        Ok(GoalTurnSettlement {
            audit,
            replayed: false,
            current_goal: Some(goal),
        })
    }
}

/// Native current scope for a production model invocation. Transferred report
/// origins are not tool authority. The cursor binder itself remains fence-agnostic.
fn tool_scope_in(
    connection: &Connection,
    context: &ToolContext,
) -> Result<zuno_db::session_work_cycle::SessionWorkCycle, GoalError> {
    let snapshot = context
        .orchestration_snapshot()
        .ok_or(GoalError::GoalTurnContextMissing)?;
    if snapshot.owner.session_id != context.session_id {
        return Err(turn_conflict(
            &context.session_id,
            GoalTurnConflictReason::AttemptSessionMismatch,
        ));
    }
    let cycle_id = snapshot
        .cycle_id
        .as_deref()
        .ok_or(GoalError::GoalTurnContextMissing)?;
    for (field, value) in [
        ("cycle_id", cycle_id),
        ("turn_id", snapshot.turn_id.as_str()),
    ] {
        if value.trim().is_empty() || value.len() > 512 {
            return Err(GoalError::InvalidGoalTurnIdentity { field });
        }
    }
    let scope = zuno_db::session_work_cycle::current_in(connection, &context.session_id)?
        .ok_or_else(|| {
            turn_conflict(
                &context.session_id,
                GoalTurnConflictReason::CycleUnavailable,
            )
        })?;
    if scope.cycle_id != cycle_id {
        return Err(turn_conflict(
            &context.session_id,
            GoalTurnConflictReason::CycleChanged,
        ));
    }
    if scope.stopped.is_some() {
        return Err(turn_conflict(
            &context.session_id,
            GoalTurnConflictReason::CycleStopped,
        ));
    }
    if scope.active_turn_id.as_deref() != Some(snapshot.turn_id.as_str()) {
        return Err(turn_conflict(
            &context.session_id,
            GoalTurnConflictReason::AttemptTurnMismatch,
        ));
    }
    // The host fence records the last started engine turn until another starts.
    // A durable settlement proves that even this still-matching turn is closed;
    // it cannot create a replacement Goal under a new Goal ID.
    let settled: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM goal_turn_audit \
         WHERE session_id=?1 AND cycle_id=?2 AND turn_id=?3)",
            params![context.session_id, cycle_id, snapshot.turn_id],
            |row| row.get(0),
        )
        .map_err(zuno_db::map_error)?;
    if settled {
        return Err(GoalError::GoalTurnAlreadySettled {
            turn_id: snapshot.turn_id.clone(),
        });
    }
    Ok(scope)
}

fn matching_goal(
    connection: &Connection,
    session_id: &str,
    identity: &GoalTurnIdentity,
) -> Result<Goal, GoalError> {
    let cycle = zuno_db::session_work_cycle::current_in(connection, session_id)?
        .ok_or_else(|| turn_conflict(session_id, GoalTurnConflictReason::CycleUnavailable))?;
    if cycle.cycle_id != identity.cycle_id {
        return Err(turn_conflict(
            session_id,
            GoalTurnConflictReason::CycleChanged,
        ));
    }
    if cycle.stopped.is_some() {
        return Err(turn_conflict(
            session_id,
            GoalTurnConflictReason::CycleStopped,
        ));
    }
    if cycle.goal_id.as_deref() != Some(identity.goal_id.as_str()) {
        return Err(turn_conflict(
            session_id,
            GoalTurnConflictReason::GoalUnowned,
        ));
    }
    GoalStore::goal_in(connection, session_id)?
        .filter(|goal| goal.goal_id == identity.goal_id)
        .ok_or_else(|| turn_conflict(session_id, GoalTurnConflictReason::GoalReplaced))
}

fn matching_cursor(
    connection: &Connection,
    session_id: &str,
    identity: &GoalTurnIdentity,
) -> Result<CycleFailure, GoalError> {
    match cycle_failure_in(connection, session_id)? {
        Some(row) if &row.identity == identity => Ok(row),
        Some(_) => Err(turn_conflict(
            session_id,
            GoalTurnConflictReason::TurnSuperseded,
        )),
        None => Err(turn_conflict(
            session_id,
            GoalTurnConflictReason::BindingInvalidated,
        )),
    }
}

fn cycle_failure_in(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<CycleFailure>, GoalError> {
    let row = connection
        .query_row(
            "SELECT goal_id,cycle_id,active_turn_id,signal,consecutive_turns \
         FROM goal_cycle_failure WHERE session_id=?1",
            params![session_id],
            |row| {
                Ok(CycleFailure {
                    identity: GoalTurnIdentity {
                        goal_id: row.get(0)?,
                        cycle_id: row.get(1)?,
                        turn_id: row.get(2)?,
                    },
                    signal: row.get(3)?,
                    consecutive_turns: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(zuno_db::map_error)?;
    if let Some(row) = &row {
        row.identity.validate()?;
        if row.consecutive_turns > crate::BLOCKED_TURN_THRESHOLD
            || row.signal.is_some() != (row.consecutive_turns > 0)
            || row
                .signal
                .as_ref()
                .is_some_and(|signal| signal.trim().is_empty())
        {
            return Err(GoalError::GoalTurnAuditCorrupt {
                session_id: session_id.to_owned(),
            });
        }
    }
    Ok(row)
}

fn audit_in(
    connection: &Connection,
    session_id: &str,
    identity: &GoalTurnIdentity,
) -> Result<Option<GoalTurnAudit>, GoalError> {
    let json: Option<String> = connection.query_row(
        "SELECT audit FROM goal_turn_audit WHERE session_id=?1 AND goal_id=?2 AND cycle_id=?3 AND turn_id=?4",
        params![session_id, identity.goal_id, identity.cycle_id, identity.turn_id],
        |row| row.get(0),
    ).optional().map_err(zuno_db::map_error)?;
    let Some(json) = json else {
        return Ok(None);
    };
    let corrupt = || GoalError::GoalTurnAuditCorrupt {
        session_id: session_id.to_owned(),
    };
    let audit: GoalTurnAudit = serde_json::from_str(&json).map_err(|_| corrupt())?;
    let valid = match audit.disposition {
        GoalTurnDisposition::Reset => {
            audit.completed_turn_streak == 0 && audit.status == GoalStatus::Active
        }
        GoalTurnDisposition::Pending => {
            (1..crate::BLOCKED_TURN_THRESHOLD).contains(&audit.completed_turn_streak)
                && audit.status == GoalStatus::Active
        }
        GoalTurnDisposition::Blocked => {
            audit.completed_turn_streak == crate::BLOCKED_TURN_THRESHOLD
                && audit.status == GoalStatus::Blocked
        }
        GoalTurnDisposition::Inactive => audit.status != GoalStatus::Active,
    };
    if !valid
        || audit.session_id != session_id
        || &audit.identity != identity
        || audit.goal_revision < 1
        || audit.completed_turn_streak > crate::BLOCKED_TURN_THRESHOLD
        || audit.signal.is_some() != (audit.completed_turn_streak > 0)
        || audit
            .signal
            .as_ref()
            .is_some_and(|signal| signal.trim().is_empty())
    {
        return Err(corrupt());
    }
    Ok(Some(audit))
}

fn refuse_settled(
    connection: &Connection,
    session_id: &str,
    identity: &GoalTurnIdentity,
) -> Result<(), GoalError> {
    if audit_in(connection, session_id, identity)?.is_some() {
        return Err(GoalError::GoalTurnAlreadySettled {
            turn_id: identity.turn_id.clone(),
        });
    }
    Ok(())
}

fn turn_conflict(session_id: &str, reason: GoalTurnConflictReason) -> GoalError {
    GoalError::GoalTurnConflict {
        session_id: session_id.to_owned(),
        reason,
    }
}

/// Native lifecycle invalidation preserves historical observations and receipts.
pub(super) fn clear_cursor_in(tx: &Transaction<'_>, session_id: &str) -> Result<(), DbError> {
    if table_exists(tx, "goal_cycle_failure")? {
        tx.execute(
            "DELETE FROM goal_cycle_failure WHERE session_id=?1",
            params![session_id],
        )
        .map_err(zuno_db::map_error)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "goal_turn_tests.rs"]
mod tests;

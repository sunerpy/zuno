//! Native boundary between accepting a new user request and resuming old work.

use super::*;
use zuno_db::session_work_cycle::{self, CycleStop, SessionWorkCycle};

impl SessionControlService {
    /// Only called by native controls after their full revision/permission audit.
    /// The previous stop remains in the append-only event log.
    pub(crate) fn authorize_cycle_in(
        tx: &zuno_db::Transaction<'_>,
        session_id: &str,
        token: &ContinuationToken,
        at_ms: i64,
    ) -> Result<(), SessionControlError> {
        let goal = GoalStore::goal_in(tx, session_id)?
            .filter(|goal| goal.status == zuno_goal::GoalStatus::Active);
        let mut cycle = session_work_cycle::read_in(tx, session_id, &token.cycle_id)?
            .unwrap_or_else(|| SessionWorkCycle {
                session_id: session_id.to_owned(),
                cycle_id: token.cycle_id.clone(),
                anchor_message_id: token.anchor_message_id.clone(),
                goal_id: None,
                plan_id: None,
                todo_ids: Default::default(),
                active_turn_id: None,
                resumed_goal_cycles: Default::default(),
                stopped: None,
                scheduling: None,
            });
        cycle.goal_id = goal.map(|goal| goal.goal_id);
        cycle.plan_id = token.plan_id.clone();
        cycle.stopped = None;
        cycle.scheduling = None;
        session_work_cycle::save_in(tx, &cycle, at_ms)?;
        zuno_db::event_log::append_in(
            tx,
            session_id,
            zuno_db::event_log::NewSessionEvent::new(
                "session.work_cycle.authorized",
                json!({"cycle":cycle,"time":at_ms})
                    .as_object()
                    .expect("object")
                    .clone(),
            )?,
        )?;
        Ok(())
    }

    pub(crate) fn transfer_goal_reports_in(
        tx: &zuno_db::Transaction<'_>,
        session_id: &str,
        goal_id: &str,
        cycle_id: &str,
        at_ms: i64,
    ) -> Result<(), SessionControlError> {
        let Some(mut current) = session_work_cycle::read_in(tx, session_id, cycle_id)? else {
            return Ok(());
        };
        if current.goal_id.as_deref() != Some(goal_id) || current.stopped.is_some() {
            return Ok(());
        }
        let mut statement = tx
            .prepare(
                "SELECT cycle_id FROM session_work_cycle WHERE session_id=?1 \
             AND json_extract(data,'$.goalId')=?2 AND cycle_id<>?3",
            )
            .map_err(open::map_error)?;
        let origins = statement
            .query_map(rusqlite::params![session_id, goal_id, cycle_id], |row| {
                row.get::<_, String>(0)
            })
            .map_err(open::map_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(open::map_error)?;
        current.resumed_goal_cycles.extend(origins);
        session_work_cycle::save_in(tx, &current, at_ms)?;
        zuno_db::event_log::append_in(tx, session_id,
            zuno_db::event_log::NewSessionEvent::new("session.goal.report_cycles_transferred",
                json!({"goalId":goal_id,"cycleId":cycle_id,"origins":current.resumed_goal_cycles,"time":at_ms})
                    .as_object().expect("object").clone())?)?;
        Ok(())
    }

    /// Bind the host's actual engine turn under the same writer snapshot as Goal,
    /// execution and cycle ownership; tools never supply these coordinates.
    pub fn begin_engine_turn(
        &self,
        session_id: &str,
        cycle_id: &str,
        turn_id: &str,
    ) -> Result<Option<zuno_goal::GoalTurnIdentity>, SessionControlError> {
        self.pool.try_transaction(|tx| {
            let Some(mut scope) = session_work_cycle::current_in(tx, session_id)? else {
                return Ok(None);
            };
            if scope.cycle_id != cycle_id || scope.stopped.is_some() {
                return Ok(None);
            }
            scope.active_turn_id = Some(turn_id.to_owned());
            session_work_cycle::save_in(tx, &scope, zuno_db::message::now_millis())?;
            let Some(goal) = GoalStore::goal_in(tx, session_id)?.filter(|goal| {
                goal.status == zuno_goal::GoalStatus::Active
                    && scope.goal_id.as_deref() == Some(&goal.goal_id)
            }) else {
                return Ok(None);
            };
            let identity = zuno_goal::GoalTurnIdentity::new(goal.goal_id, cycle_id, turn_id)?;
            let previous = GoalStore::current_goal_turn_in(tx, session_id)?;
            GoalStore::bind_goal_turn_in(tx, session_id, &identity, previous.as_ref())?;
            Ok(Some(identity))
        })
    }

    /// Activate a persisted user message at promotion, in the transaction which
    /// consumes it. Busy input admission must not call this or replace T1's scope.
    #[allow(
        clippy::too_many_arguments,
        reason = "promotion binds the persisted input, message and execution identity in one transaction"
    )]
    pub fn activate_user_input_in(
        tx: &zuno_db::Transaction<'_>,
        session_id: &str,
        input_id: &str,
        message_id: &str,
        mode: CollaborationMode,
        identity: TurnExecutionIdentity,
        at_ms: i64,
    ) -> Result<SessionWorkCycle, SessionControlError> {
        // Stable identity makes restart/retry of the same promoted input idempotent.
        let cycle_id = format!("input_{message_id}");
        if let Some(cycle) = session_work_cycle::read_in(tx, session_id, &cycle_id)? {
            return Ok(cycle);
        }
        let input = zuno_db::inbox::read_in(tx, session_id, input_id)?.ok_or_else(|| {
            SessionControlError::CorruptState {
                session_id: session_id.to_owned(),
                detail: "user promotion has no durable input".to_owned(),
            }
        })?;
        if input.state != zuno_db::inbox::SubmissionState::Promoted
            || !matches!(
                zuno_db::inbox::DurableInputKind::classify(&input.prompt),
                Some(
                    zuno_db::inbox::DurableInputKind::User
                        | zuno_db::inbox::DurableInputKind::TuiPrompt
                        | zuno_db::inbox::DurableInputKind::AcpPrompt
                        | zuno_db::inbox::DurableInputKind::HostMessage
                )
            )
        {
            return Err(SessionControlError::CorruptState {
                session_id: session_id.to_owned(),
                detail: "only a promoted real user input may start user work".to_owned(),
            });
        }
        let mut state = seed_in(tx, session_id, mode, Some(identity.clone()), at_ms)?;
        let previous_cycle_id = state.cycle_id.clone();
        let previous_scheduling = state.scheduling.clone();
        let prior = session_work_cycle::current_in(tx, session_id)?;
        let prior_scope = prior.clone();
        let proven_cancel = prior.as_ref().is_some_and(|cycle| {
            cycle
                .stopped
                .as_ref()
                .is_some_and(|stop| stop.user_cancelled)
        }) || (prior.is_none()
            && legacy_user_cancel_in(tx, &state, message_id)?);
        let uncertain = !zuno_db::message::MessageStore::new(tx)
            .pending_uncertain_tool_calls(session_id, 0)?
            .is_empty();
        let protected =
            state
                .scheduling
                .as_ref()
                .is_some_and(|scheduling| match scheduling.readiness {
                    SessionReadiness::WaitingHuman { .. } => true,
                    SessionReadiness::Paused { reason } => match reason {
                        SessionPauseReason::User => !proven_cancel,
                        SessionPauseReason::Authentication
                        | SessionPauseReason::TurnBudget
                        | SessionPauseReason::UncertainSideEffect
                        | SessionPauseReason::Blocked => true,
                        SessionPauseReason::NoProgress | SessionPauseReason::NoExecutableWork => {
                            false
                        }
                    },
                    _ => false,
                })
                || (state.scheduling.is_none()
                    && matches!(
                        state.phase,
                        SessionExecutionPhase::Paused
                            | SessionExecutionPhase::Blocked
                            | SessionExecutionPhase::Waiting
                    ))
                || uncertain;
        if let Some(mut previous) = prior {
            previous.scheduling = state.scheduling.clone();
            session_work_cycle::save_in(tx, &previous, at_ms)?;
        }
        let goal = GoalStore::goal_in(tx, session_id)?
            .filter(|goal| goal.status == zuno_goal::GoalStatus::Active);
        let plan = WorkStateStore::plan_in(tx, session_id)?;
        let cycle = SessionWorkCycle {
            session_id: session_id.to_owned(),
            cycle_id: cycle_id.clone(),
            anchor_message_id: Some(message_id.to_owned()),
            goal_id: goal.as_ref().map(|goal| goal.goal_id.clone()),
            // Explicit Plan mode owns planning. An active Goal already owns its
            // declared work. A paused Goal or ordinary new request owns neither.
            plan_id: plan
                .as_ref()
                .filter(|plan| {
                    state.mode == CollaborationMode::Plan
                        || goal.as_ref().is_some_and(|goal| {
                            plan.goal_id.as_deref() == Some(goal.goal_id.as_str())
                        })
                })
                .map(|plan| plan.id.clone()),
            todo_ids: Default::default(),
            active_turn_id: None,
            resumed_goal_cycles: goal
                .as_ref()
                .and_then(|goal| {
                    prior_scope.as_ref().filter(|scope| {
                        scope.goal_id.as_deref() == Some(&goal.goal_id) && scope.stopped.is_none()
                    })
                })
                .map(|scope| {
                    let mut origins = scope.resumed_goal_cycles.clone();
                    origins.insert(scope.cycle_id.clone());
                    origins
                })
                .unwrap_or_default(),
            stopped: None,
            scheduling: None,
        };
        if !protected {
            state.scheduling = Some(SessionScheduling::default());
            state.phase = if state.mode == CollaborationMode::Plan {
                SessionExecutionPhase::Planning
            } else {
                SessionExecutionPhase::Running
            };
        } else if uncertain
            && state.scheduling.as_ref().is_none_or(|s| {
                matches!(
                    s.readiness,
                    SessionReadiness::Ready | SessionReadiness::Completed
                )
            })
        {
            let scheduling = state.scheduling.get_or_insert_default();
            scheduling.readiness = SessionReadiness::Paused {
                reason: SessionPauseReason::UncertainSideEffect,
            };
            state.phase = SessionExecutionPhase::Paused;
        }
        state.cycle_id = Some(cycle_id.clone());
        // A previous continuation must not carry its old Plan/anchor into this input.
        let context_epoch = state
            .continuation
            .as_ref()
            .map_or(0, |token| token.context_epoch);
        state.continuation = Some(ContinuationToken {
            cycle_id,
            identity,
            mode: state.mode,
            plan_id: cycle.plan_id.clone(),
            plan_revision: plan
                .as_ref()
                .filter(|plan| cycle.plan_id.as_deref() == Some(&plan.id))
                .map(|plan| plan.revision),
            context_epoch,
            anchor_message_id: Some(message_id.to_owned()),
        });
        let revision = state.revision;
        state.time_updated = at_ms;
        update_in(tx, revision, state)?;
        session_work_cycle::save_in(tx, &cycle, at_ms)?;
        tx.execute(
            "UPDATE session_input SET cycle_id=?1,revision=revision+1 \
             WHERE session_id=?2 AND id=?3 AND revision=?4",
            rusqlite::params![cycle.cycle_id, session_id, input_id, input.revision],
        )
        .map_err(open::map_error)?;
        zuno_db::event_log::append_in(
            tx,
            session_id,
            zuno_db::event_log::NewSessionEvent::new(
                "session.work_cycle.started",
                json!({"cycle": cycle, "inputId": input_id, "time": at_ms,
                    "previousCycleId":previous_cycle_id,"previousScheduling":previous_scheduling,
                    "legacyInterruption":prior_scope.is_none() && proven_cancel,
                    "protectedGateRetained":protected,
                })
                .as_object()
                .expect("object")
                .clone(),
            )?,
        )?;
        Ok(cycle)
    }

    /// Stop the exact old cycle. A late stop never pauses the newer current row.
    pub fn stop_turn(
        &self,
        session_id: &str,
        cycle_id: &str,
        turn_id: &str,
        user_cancelled: bool,
        at_ms: i64,
    ) -> Result<(), SessionControlError> {
        self.stop_cycle(session_id, cycle_id, Some(turn_id), user_cancelled, at_ms)
    }

    /// A cancellation during prelude has a durable input, but no provider turn.
    pub fn stop_input(
        &self,
        session_id: &str,
        cycle_id: &str,
        user_cancelled: bool,
        at_ms: i64,
    ) -> Result<(), SessionControlError> {
        self.stop_cycle(session_id, cycle_id, None, user_cancelled, at_ms)
    }

    fn stop_cycle(
        &self,
        session_id: &str,
        cycle_id: &str,
        turn_id: Option<&str>,
        user_cancelled: bool,
        at_ms: i64,
    ) -> Result<(), SessionControlError> {
        self.pool.try_transaction(|tx| {
            let Some(mut cycle) = session_work_cycle::read_in(tx, session_id, cycle_id)? else {
                return Ok(());
            };
            if cycle.stopped.is_some() {
                return Ok(());
            }
            if turn_id.is_some_and(|turn_id| cycle.active_turn_id.as_deref() != Some(turn_id)) {
                // A late terminal callback from T1 cannot stop T2, including
                // same-cycle foreground continuation and compaction recovery.
                return Ok(());
            }
            use rusqlite::OptionalExtension as _;
            let input_id = tx
                .query_row(
                "SELECT i.id FROM session_input i JOIN session_input_receipt r ON r.input_id=i.id \
                 WHERE i.session_id=?1 AND i.cycle_id=?2 \
                   AND (r.turn_id=?3 OR (?3 IS NULL AND r.turn_id IS NULL)) \
                 ORDER BY i.admitted_seq DESC LIMIT 1",
                rusqlite::params![session_id, cycle_id, turn_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(open::map_error)?;
            cycle.stopped = Some(CycleStop {
                turn_id: turn_id.map(str::to_owned),
                input_id,
                user_cancelled,
                at_ms,
            });
            let current = read_in(tx, session_id)?;
            if let Some(state) = current.filter(|state| state.cycle_id.as_deref() == Some(cycle_id))
            {
                cycle.scheduling = state.scheduling.clone();
                // Preserve real barriers. Otherwise stopping this cycle closes it,
                // not the conversation; automatic wake rejects Completed.
                if state
                    .scheduling
                    .as_ref()
                    .is_none_or(|s| s.readiness == SessionReadiness::Ready)
                {
                    zuno_db::session_execution::set_scheduling_in(
                        tx,
                        session_id,
                        state.revision,
                        SessionScheduling {
                            readiness: SessionReadiness::Completed,
                            ..Default::default()
                        },
                        at_ms,
                    )?;
                }
            }
            session_work_cycle::save_in(tx, &cycle, at_ms)?;
            zuno_db::event_log::append_in(
                tx,
                session_id,
                zuno_db::event_log::NewSessionEvent::new(
                    "session.work_cycle.stopped",
                    json!({"cycle":cycle,"time":at_ms})
                        .as_object()
                        .expect("object")
                        .clone(),
                )?,
            )?;
            Ok(())
        })
    }
}

/// Only typed interrupted assistant checkpoints from the released host count.
/// Inspect the last assistant before this input; never match natural-language text
/// or use an older interruption to clear a later explicit pause.
fn legacy_user_cancel_in(
    connection: &zuno_db::Connection,
    state: &SessionExecutionState,
    message_id: &str,
) -> Result<bool, zuno_error::DbError> {
    use rusqlite::OptionalExtension as _;
    let Some(anchor) = state
        .continuation
        .as_ref()
        .and_then(|token| token.anchor_message_id.as_deref())
    else {
        return Ok(false);
    };
    struct LegacyStopEvidence {
        name: Option<String>,
        reason: Option<String>,
        parent: Option<String>,
        turn: Option<String>,
    }
    let proof: Option<LegacyStopEvidence> = connection
        .query_row(
            "SELECT json_extract(data,'$.error.name'),json_extract(data,'$.error.data.reason'), \
         json_extract(data,'$.parentID'),json_extract(data,'$.turnID') \
         FROM message WHERE session_id=?1 AND id<>?2 AND json_extract(data,'$.role')='assistant' \
         ORDER BY time_created DESC,id DESC LIMIT 1",
            rusqlite::params![state.session_id, message_id],
            |row| {
                Ok(LegacyStopEvidence {
                    name: row.get(0)?,
                    reason: row.get(1)?,
                    parent: row.get(2)?,
                    turn: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(open::map_error)?;
    let latest_turn: Option<String> = connection
        .query_row(
            "SELECT json_extract(data,'$.turnID') FROM event WHERE aggregate_id=?1 \
         AND type='session.turn.started.1' ORDER BY seq DESC LIMIT 1",
            [&state.session_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(open::map_error)?
        .flatten();
    Ok(proof.is_some_and(|proof| {
        proof.name.as_deref() == Some("AbortError")
            && matches!(
                proof.reason.as_deref(),
                Some("user_cancel" | "request_cancelled")
            )
            && proof.parent.as_deref() == Some(anchor)
            && proof.turn.is_some()
            && proof.turn == latest_turn
    }))
}

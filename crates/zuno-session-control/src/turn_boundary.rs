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
                input.trigger_kind,
                InputTriggerKind::User | InputTriggerKind::Legacy
            )
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
        let legacy_interruption = legacy_user_cancel_in(tx, &state)?;
        let proven_cancel = prior.as_ref().is_some_and(|cycle| {
            cycle
                .stopped
                .as_ref()
                .is_some_and(|stop| stop.user_cancelled)
        }) || legacy_interruption.is_some();
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
                    "legacyInterruption":legacy_interruption.is_some(),
                    "legacyInterruptionProof":legacy_interruption.as_ref().map(LegacyInterruptionProof::event_data),
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

struct LegacyInterruptionProof {
    origin_cycle_id: String,
    turn_id: String,
    executing_sequence: i64,
    turn_started_sequence: i64,
    cancellation_sequence: i64,
    paused_sequence: i64,
}

impl LegacyInterruptionProof {
    fn event_data(&self) -> serde_json::Value {
        json!({
            "originCycleId":self.origin_cycle_id,
            "turnId":self.turn_id,
            "executingSequence":self.executing_sequence,
            "turnStartedSequence":self.turn_started_sequence,
            "cancellationSequence":self.cancellation_sequence,
            "pausedSequence":self.paused_sequence,
        })
    }
}

struct LegacyBoundaryEvent {
    sequence: i64,
    kind: String,
    data: serde_json::Value,
}

/// Audit only on promotion of a real user input. An automatic wake cannot call
/// this repair. The latest driver pause, not an arbitrary historical AbortError,
/// must project the original turn stop. v31 could first project that pause while
/// completing a later user turn. This proof only applies before any native input
/// cycle was created. A v32 retained-gate cycle overwrote the previous state's
/// timestamp without recording pause provenance; it requires explicit resume.
///
/// Missing, unknown-version, changed or overly long provenance stays gated.
/// This is a bounded read of native lifecycle facts, never assistant prose,
/// a database migration, or permission to replay an earlier failed input.
/// In particular, the old latest-Abort/turn/anchor-only shortcut cannot prove
/// pause origin: a later independent pause leaves those same three values.
/// Fully evidenced latest interruptions still qualify; incomplete old records
/// require explicit native resume instead of silently acquiring authority.
fn legacy_user_cancel_in(
    connection: &zuno_db::Connection,
    state: &SessionExecutionState,
) -> Result<Option<LegacyInterruptionProof>, zuno_error::DbError> {
    let Some(scheduling) = state.scheduling.as_ref().filter(|scheduling| {
        state.phase == SessionExecutionPhase::Paused
            && scheduling.readiness
                == (SessionReadiness::Paused {
                    reason: SessionPauseReason::User,
                })
    }) else {
        return Ok(None);
    };
    let Some(token) = state
        .continuation
        .as_ref()
        .filter(|token| state.cycle_id.as_deref() == Some(&token.cycle_id))
    else {
        return Ok(None);
    };
    // Reject before searching historical cancellations. A retained v32 cycle
    // cannot distinguish the old interruption from a later same-valued pause.
    if session_work_cycle::current_in(connection, &state.session_id)?.is_some() {
        return Ok(None);
    }
    // This compatibility proof is for ordinary conversations with no human
    // authority history. A cancelled required question also leaves paused/user.
    // Legacy records cannot prove its pause was superseded merely because the
    // request is no longer pending or a v32 bridge changed time_updated.
    let other_authority: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM goal WHERE session_id=?1) \
                 OR EXISTS(SELECT 1 FROM human_request WHERE session_id=?1)",
            [&state.session_id],
            |row| row.get(0),
        )
        .map_err(open::map_error)?;
    if other_authority {
        return Ok(None);
    }

    let Some(events) = legacy_boundary_events_in(connection, &state.session_id)? else {
        return Ok(None);
    };
    let mut phases = events
        .iter()
        .rev()
        .filter(|event| event.kind.starts_with("session.driver.phase."));
    let (Some(paused), Some(executing)) = (phases.next(), phases.next()) else {
        return Ok(None);
    };
    let Some(origin) = paused.data["cycleId"].as_str() else {
        return Ok(None);
    };
    if paused.kind != "session.driver.phase.1"
        || paused.data["phase"] != "paused"
        || paused.data["reason"] != "user"
        || paused.data["pauseReason"] != "user"
        || executing.kind != "session.driver.phase.1"
        || executing.data["phase"] != "executing"
        || executing.data["cycleId"] != origin
        || !executing.data["pauseReason"].is_null()
        || (!paused.data["progressFingerprint"].is_null()
            && !paused.data["progressFingerprint"].is_string())
        || (!paused.data["unchangedProgressCount"].is_null()
            && paused.data["unchangedProgressCount"].as_u64().is_none())
        || paused.data["progressFingerprint"].as_str() != scheduling.progress_fingerprint.as_deref()
        || paused.data["unchangedProgressCount"].as_u64().unwrap_or(0)
            != u64::from(scheduling.unchanged_progress_count)
        || token.cycle_id != origin
    {
        return Ok(None);
    }
    let start = events.iter().find(|event| {
        event.sequence > executing.sequence
            && event.sequence < paused.sequence
            && event.kind.starts_with("session.turn.started.")
    });
    let Some(start) = start else {
        return Ok(None);
    };
    let (Some(turn), Some(anchor)) = (
        start.data["turnID"].as_str(),
        start.data["anchorMessageID"].as_str(),
    ) else {
        return Ok(None);
    };
    if start.kind != "session.turn.started.1"
        || !legacy_turn_receipt_in(connection, &state.session_id, start, "cancelled")?
        || !legacy_cancel_checkpoint_in(connection, &state.session_id, turn, anchor)?
    {
        return Ok(None);
    }
    let Some(cancelled) = events.iter().find(|event| {
        event.sequence > start.sequence
            && event.sequence < paused.sequence
            && event.kind == "session.input.receipt.1"
            && event.data["turnId"] == turn
            && event.data["inputId"] == anchor
            && event.data["state"] == "cancelled"
            && event.data["stopReason"] == "cancelled"
    }) else {
        return Ok(None);
    };

    let mut current_anchor = anchor.to_owned();
    let mut last_terminal_sequence = cancelled.sequence;
    let mut last_followup_turn = None;
    for event in events
        .iter()
        .filter(|event| event.sequence > start.sequence)
    {
        if event.kind.starts_with("question.") || event.kind.starts_with("session.work_cycle.") {
            // A later native authority/cycle event breaks the direct proof,
            // even if its materialized row is absent. Never infer missing pause
            // provenance by following previousCycleId/previousScheduling.
            return Ok(None);
        } else if event.kind.starts_with("session.turn.started.") {
            // The v31 host could answer subsequent user queries without changing
            // the original paused row. They must really have completed.
            if event.kind != "session.turn.started.1"
                || event.sequence <= last_terminal_sequence
                || !legacy_turn_receipt_in(connection, &state.session_id, event, "completed")?
            {
                return Ok(None);
            }
            let Some(completed) = events.iter().find(|receipt| {
                receipt.sequence > event.sequence
                    && receipt.kind == "session.input.receipt.1"
                    && receipt.data["inputId"] == event.data["anchorMessageID"]
                    && receipt.data["turnId"] == event.data["turnID"]
                    && receipt.data["state"] == "completed"
                    && receipt.data["stopReason"] == "end_turn"
            }) else {
                return Ok(None);
            };
            last_terminal_sequence = completed.sequence;
            last_followup_turn = event.data["turnID"].as_str();
            current_anchor = event.data["anchorMessageID"]
                .as_str()
                .expect("validated user anchor")
                .to_owned();
        }
    }
    if token.anchor_message_id.as_deref() != Some(current_anchor.as_str()) {
        return Ok(None);
    }
    if let Some(turn) = last_followup_turn {
        // v31 record_continuation changes anchor/time during the user prelude,
        // while leaving the scheduling gate alone. A later unexplained state
        // write is not attributable to that prelude. This does not invent a
        // timestamp for the original pause.
        let prelude: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM session_input i JOIN session_input_receipt r ON r.input_id=i.id \
             WHERE i.session_id=?1 AND i.id=?2 AND r.turn_id=?3 \
               AND i.time_created<=?4 AND r.applied_at>=?4)",
            rusqlite::params![state.session_id,current_anchor,turn,state.time_updated],
            |row| row.get(0),
        ).map_err(open::map_error)?;
        if !prelude {
            return Ok(None);
        }
    }
    Ok(Some(LegacyInterruptionProof {
        origin_cycle_id: origin.to_owned(),
        turn_id: turn.to_owned(),
        executing_sequence: executing.sequence,
        turn_started_sequence: start.sequence,
        cancellation_sequence: cancelled.sequence,
        paused_sequence: paused.sequence,
    }))
}

/// Bound both memory and provenance work. If the relevant origin was pruned or
/// lies beyond this window, explicit native resume remains the safe path.
fn legacy_boundary_events_in(
    connection: &zuno_db::Connection,
    session_id: &str,
) -> Result<Option<Vec<LegacyBoundaryEvent>>, zuno_error::DbError> {
    const MAX_EVENTS: usize = 1_024;
    let mut statement = connection
        .prepare(
            "SELECT seq,type,data FROM event WHERE aggregate_id=?1 AND ( \
               (type>='session.driver.phase.' AND type<'session.driver.phase/') OR \
               (type>='session.turn.started.' AND type<'session.turn.started/') OR \
               (type>='session.input.receipt.' AND type<'session.input.receipt/') OR \
               (type>='question.' AND type<'question/') OR \
               (type>='session.work_cycle.' AND type<'session.work_cycle/')) \
             ORDER BY seq DESC LIMIT 1025",
        )
        .map_err(open::map_error)?;
    let rows = statement
        .query_map([session_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(open::map_error)?;
    let mut events = Vec::new();
    for row in rows {
        let (sequence, kind, data) = row.map_err(open::map_error)?;
        let Ok(data) = serde_json::from_str(&data) else {
            return Ok(None);
        };
        if events.len() == MAX_EVENTS {
            return Ok(None);
        }
        events.push(LegacyBoundaryEvent {
            sequence,
            kind,
            data,
        });
    }
    events.reverse();
    Ok(Some(events))
}

fn legacy_turn_receipt_in(
    connection: &zuno_db::Connection,
    session_id: &str,
    start: &LegacyBoundaryEvent,
    expected_state: &str,
) -> Result<bool, zuno_error::DbError> {
    let (Some(turn), Some(anchor)) = (
        start.data["turnID"].as_str(),
        start.data["anchorMessageID"].as_str(),
    ) else {
        return Ok(false);
    };
    if start.data["turnTrigger"] != "user" {
        return Ok(false);
    }
    let Some(input) = zuno_db::inbox::read_in(connection, session_id, anchor)? else {
        return Ok(false);
    };
    if !legacy_real_user(&input)
        || input.state != zuno_db::inbox::SubmissionState::Consumed
        || input.cycle_id.is_some()
        || input.admitted_sequence >= start.sequence
    {
        return Ok(false);
    }
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM session_input_receipt r JOIN message m ON m.id=r.input_id \
             WHERE r.input_id=?1 AND m.session_id=?2 AND json_extract(m.data,'$.role')='user' \
               AND r.turn_id=?3 AND r.state=?4 AND r.applied_at IS NOT NULL \
               AND r.completed_at IS NOT NULL AND r.stop_reason=?5 AND r.error IS NULL)",
            rusqlite::params![
                anchor,
                session_id,
                turn,
                expected_state,
                if expected_state == "cancelled" {
                    "cancelled"
                } else {
                    "end_turn"
                },
            ],
            |row| row.get(0),
        )
        .map_err(open::map_error)
}

fn legacy_cancel_checkpoint_in(
    connection: &zuno_db::Connection,
    session_id: &str,
    turn_id: &str,
    anchor: &str,
) -> Result<bool, zuno_error::DbError> {
    use rusqlite::OptionalExtension as _;
    let checkpoint: Option<String> = connection
        .query_row(
            "SELECT data FROM message WHERE session_id=?1 \
               AND json_extract(data,'$.role')='assistant' \
               AND json_extract(data,'$.turnID')=?2 ORDER BY time_created DESC,id DESC LIMIT 1",
            [session_id, turn_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(open::map_error)?;
    let Some(checkpoint) =
        checkpoint.and_then(|data| serde_json::from_str::<serde_json::Value>(&data).ok())
    else {
        return Ok(false);
    };
    Ok(checkpoint["parentID"] == anchor
        && checkpoint["error"]["name"] == "AbortError"
        && matches!(
            checkpoint["error"]["data"]["reason"].as_str(),
            Some("user_cancel" | "request_cancelled")
        )
        && matches!(
            checkpoint["error"]["data"]["source"].as_str(),
            Some("acp" | "tui" | "api")
        ))
}

fn legacy_real_user(input: &zuno_db::inbox::SessionInput) -> bool {
    use zuno_db::inbox::DurableInputKind as Kind;
    // Released ACP/TUI producers used NewSessionInput's Legacy default.
    // Executed turns additionally require turnTrigger=user.
    matches!(
        (input.trigger_kind, Kind::classify(&input.prompt)),
        (
            InputTriggerKind::User,
            Some(Kind::User | Kind::TuiPrompt | Kind::AcpPrompt | Kind::HostMessage)
        ) | (
            InputTriggerKind::Legacy,
            Some(Kind::User | Kind::TuiPrompt | Kind::AcpPrompt)
        )
    )
}

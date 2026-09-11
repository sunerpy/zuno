//! The tool phase of a provider step. Scheduling, result ordering and safe-point
//! input consumption share this implementation for ordinary and bounded turns.

use super::*;
use zuno_types::wait::WaitRef;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingToolCall {
    pub index: usize,
    pub reference: WaitRef,
}

/// Data required to resume the tool phase without another model request.
/// Futures, provider objects, cancellation handles and host paths stay outside it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolStepCheckpoint {
    pub step: u32,
    pub assistant_id: String,
    pub assistant_time_created: i64,
    pub(crate) requested: RequestedTurn,
    pub locked_tools: Vec<ToolDefinition>,
    pub orchestration_snapshot: AttemptSnapshot,
    pub calls: Vec<ToolCall>,
    pub call_positions: Vec<usize>,
    pub finish_reason: Option<FinishReason>,
    pub next_call: usize,
    pub pending: Vec<PendingToolCall>,
    pub(crate) effects: ToolStepEffects,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ToolStepEffects {
    injected: InjectedLiveInputs,
    yield_until_input: bool,
    waiting_for_human: Option<String>,
    work_state_read: Option<(String, String, String)>,
    dynamic_context_refresh: Option<ToolDynamicContextRefresh>,
}

pub(super) enum ToolStepOutcome {
    Completed(ToolStepResult),
    Waiting,
}

pub(super) struct ToolStepResult {
    pub injected: InjectedLiveInputs,
    pub yield_until_input: bool,
    pub waiting_for_human: Option<String>,
    pub work_state_read: Option<(String, String, String)>,
    pub dynamic_context_refresh: Option<ToolDynamicContextRefresh>,
}

pub(super) async fn execute(
    phase: &mut ToolStepCheckpoint,
    request: &RunTurnRequest,
    context: &mut TurnContext<'_>,
    events: &TurnEventSender,
    store: &TurnState<'_>,
    accounting: (&mut u32, &mut BTreeMap<String, ToolFailureRecovery>),
    bounded: bool,
) -> Result<ToolStepOutcome, TurnError> {
    let (tool_calls_dispatched, unresolved_tool_failures) = accounting;
    if !phase.pending.is_empty() {
        return Err(crate::state::TurnStateError::Conflict.into());
    }
    let step = phase.step;
    let assistant_id = phase.assistant_id.as_str();
    let assistant_time_created = phase.assistant_time_created;
    let requested = &phase.requested;
    let locked_tools: Arc<[ToolDefinition]> = phase.locked_tools.clone().into();
    let orchestration_snapshot = Arc::new(phase.orchestration_snapshot.clone());
    let calls = &phase.calls;
    let call_positions = &phase.call_positions;
    let mut effects = std::mem::take(&mut phase.effects);
    effects
        .injected
        .merge(inject_live_inputs(context, request, requested, events).await?);
    if !effects.injected.skip_remaining_tools {
        let mut next_call = phase.next_call;
        while next_call < calls.len() && !effects.injected.skip_remaining_tools {
            let first_request = dispatch_request(
                calls[next_call].clone(),
                request,
                assistant_id,
                &orchestration_snapshot.agent.name,
                &locked_tools,
                context,
                &orchestration_snapshot,
            );
            let first_policy = context.dispatcher.concurrency_policy(&first_request);
            let mut group_end = next_call.saturating_add(1);
            if first_policy != ToolConcurrencyPolicy::Exclusive {
                while group_end < calls.len() {
                    let candidate = dispatch_request(
                        calls[group_end].clone(),
                        request,
                        assistant_id,
                        &orchestration_snapshot.agent.name,
                        &locked_tools,
                        context,
                        &orchestration_snapshot,
                    );
                    if context.dispatcher.concurrency_policy(&candidate)
                        == ToolConcurrencyPolicy::Exclusive
                    {
                        break;
                    }
                    group_end = group_end.saturating_add(1);
                }
            }

            let mut prepared = Vec::with_capacity(group_end.saturating_sub(next_call));
            for (call_index, call) in calls
                .iter()
                .cloned()
                .enumerate()
                .take(group_end)
                .skip(next_call)
            {
                let ui_intent = tool_ui_intent(&locked_tools, &call.name);
                let display_name = tool_display_name(&locked_tools, &call.name);
                events
                    .send(TurnEvent::ToolDispatchStarted {
                        step,
                        call_id: call.id.clone(),
                        display_name: display_name.clone(),
                        name: call.name.clone(),
                        ui_intent,
                    })
                    .await?;
                let dispatch = context
                    .dispatcher
                    .prepare(dispatch_request(
                        call.clone(),
                        request,
                        assistant_id,
                        &orchestration_snapshot.agent.name,
                        &locked_tools,
                        context,
                        &orchestration_snapshot,
                    ))
                    .await;
                let deferred = matches!(&dispatch, PreparedToolDispatch::Pending(_));
                if let PreparedToolDispatch::Pending(reference) = &dispatch {
                    if !bounded {
                        return Err(crate::state::TurnStateError::InvalidData.into());
                    }
                    crate::wait::validate_binding(reference, request, &call)?;
                }
                prepared.push((call_index, call, display_name, ui_intent, dispatch));
                if deferred {
                    // Do not prepare later calls across a durable wait. Earlier
                    // independent calls still run together; continuation metadata
                    // and result effects remain in the model's original order.
                    group_end = call_index + 1;
                    break;
                }
            }

            // The hand-off becomes durable before any of this group can take
            // effect. A process that dies inside `execute` leaves a row the next
            // turn has to classify, and the only evidence that survives the death
            // is what was committed before it: `repair_missing_tool_outputs` reads
            // this stamp to separate a call that may have changed authoritative
            // state from one that was never handed over. Written for the whole
            // group in one transaction because the group runs concurrently, so any
            // member of it may be the call that is in flight.
            mark_group_dispatched(
                store,
                request,
                step,
                &locked_tools,
                DispatchedGroup {
                    assistant_id,
                    assistant_time_created,
                    call_positions,
                    calls: prepared
                        .iter()
                        .filter(|(_, _, _, _, dispatch)| {
                            matches!(dispatch, PreparedToolDispatch::Execution(_))
                        })
                        .map(|(call_index, call, display_name, ui_intent, _)| {
                            (*call_index, call, display_name.as_str(), *ui_intent)
                        })
                        .collect(),
                },
            )
            .await?;

            let completed = if first_policy == ToolConcurrencyPolicy::Exclusive {
                let (call_index, call, display_name, ui_intent, dispatch) =
                    prepared.pop().expect("exclusive group contains one call");
                vec![(
                    call_index,
                    call,
                    display_name,
                    ui_intent,
                    dispatch.execute().await,
                )]
            } else {
                stream::iter(prepared.into_iter().map(
                    |(call_index, call, display_name, ui_intent, dispatch)| async move {
                        (
                            call_index,
                            call,
                            display_name,
                            ui_intent,
                            dispatch.execute().await,
                        )
                    },
                ))
                .buffered(context.tool_concurrency.get())
                .collect::<Vec<_>>()
                .await
            };
            let mut results = Vec::with_capacity(completed.len());
            for (index, call, display, intent, outcome) in completed {
                match outcome {
                    ToolDispatchOutcome::Completed(result) => {
                        results.push((index, call, display, intent, *result));
                    }
                    ToolDispatchOutcome::Pending(reference) => {
                        phase.pending.push(PendingToolCall { index, reference });
                    }
                }
            }
            let completed = results;
            // Every entry ran to a result, whether it succeeded, was blocked, or was
            // interrupted, so this is the truthful count of tool work the turn did;
            // the next `before_request` reads it.
            *tool_calls_dispatched = tool_calls_dispatched
                .saturating_add(u32::try_from(completed.len()).unwrap_or(u32::MAX));

            // A completed parallel group is an indivisible durable unit: every
            // execution result is appended in model order before an urgent inbox
            // item may prevent the next group from starting.
            let result_parts = completed
                .iter()
                .map(|(call_index, call, display_name, ui_intent, dispatch)| {
                    tool_result_part(
                        request,
                        ToolPartIdentity {
                            step,
                            position: call_positions[*call_index],
                            message_time_created: assistant_time_created,
                            message_id: assistant_id,
                            call,
                            display_name,
                            ui_intent: *ui_intent,
                            schema_identity: tool_schema_identity(&locked_tools, &call.name),
                        },
                        dispatch,
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;
            store
                .persistence
                .commit_tool_parts(
                    &store.scope,
                    &result_parts,
                    crate::state::ToolPartCommitKind::Result,
                    now_millis(),
                )
                .await?;
            for (_, call, display_name, _, dispatch) in completed {
                apply_result(
                    &mut effects,
                    unresolved_tool_failures,
                    &call,
                    &dispatch,
                    calls.len() == 1,
                )?;
                if let Some(kind) = dispatch.blocked {
                    events
                        .send(TurnEvent::ToolDispatchBlocked {
                            step,
                            call_id: call.id.clone(),
                            kind,
                        })
                        .await?;
                }
                if let Some(interruption) = dispatch.interruption {
                    // The dispatcher already resolved this call's certainty against
                    // the tool's own claim and recorded it; recomputing it from the
                    // mode here is what made the live surfaces contradict the
                    // durable record. The mode is only the fallback for a result
                    // that carries no readable verdict.
                    let uncertain = crate::dispatch::recorded_interruption_uncertainty(&dispatch)
                        .unwrap_or_else(|| interruption.uncertain());
                    events
                        .send(TurnEvent::ToolDispatchInterrupted {
                            step,
                            call_id: call.id.clone(),
                            display_name,
                            name: call.name,
                            title: dispatch.output.title.clone(),
                            output: dispatch.output.output.clone(),
                            interruption,
                            uncertain,
                        })
                        .await?;
                } else {
                    if let Some(presentation) = dispatch.output.presentation.clone() {
                        events
                            .send(TurnEvent::ToolResultPresented {
                                step,
                                call_id: call.id.clone(),
                                presentation,
                            })
                            .await?;
                    }
                    events
                        .send(TurnEvent::ToolDispatchCompleted {
                            step,
                            call_id: call.id.clone(),
                            display_name,
                            name: call.name,
                            title: dispatch.output.title.clone(),
                            output: dispatch.output.output.clone(),
                            diff: ToolDiff::from_output(&dispatch.output),
                            written_paths: dispatch
                                .output
                                .written_paths()
                                .into_iter()
                                .map(str::to_owned)
                                .collect(),
                            is_error: dispatch.is_error,
                        })
                        .await?;
                }
                events
                    .send(TurnEvent::ToolResultAppended {
                        step,
                        call_id: call.id,
                        is_error: dispatch.is_error,
                    })
                    .await?;
                if effects.waiting_for_human.is_some() {
                    effects.injected.skip_remaining_tools = true;
                } else {
                    effects
                        .injected
                        .merge(inject_live_inputs(context, request, requested, events).await?);
                }
            }
            next_call = group_end;
            phase.next_call = next_call;
            if !phase.pending.is_empty() {
                phase.effects = effects;
                return Ok(ToolStepOutcome::Waiting);
            }
        }
    }
    if effects.waiting_for_human.is_none() {
        effects
            .injected
            .merge(inject_live_inputs(context, request, requested, events).await?);
    }
    Ok(ToolStepOutcome::Completed(ToolStepResult {
        injected: effects.injected,
        yield_until_input: effects.yield_until_input,
        waiting_for_human: effects.waiting_for_human,
        work_state_read: effects.work_state_read,
        dynamic_context_refresh: effects.dynamic_context_refresh,
    }))
}

fn apply_result(
    effects: &mut ToolStepEffects,
    unresolved: &mut BTreeMap<String, ToolFailureRecovery>,
    call: &ToolCall,
    dispatch: &ToolDispatchResult,
    single: bool,
) -> Result<(), TurnError> {
    let successful = !dispatch.is_error
        && dispatch.recovery.is_none()
        && dispatch.blocked.is_none()
        && dispatch.interruption.is_none()
        && dispatch.uncertain.is_none();
    if single
        && successful
        && dispatch.output.continuation == ToolContinuation::Continue
        && dispatch.output.written_paths().is_empty()
        && let Some(observation) = dispatch.output.progress_observation()
    {
        effects.work_state_read = Some((
            call.name.clone(),
            observation.scope().to_owned(),
            observation.fingerprint().to_owned(),
        ));
    }
    if successful && let Some(refresh) = dispatch.output.dynamic_context_refresh() {
        effects.dynamic_context_refresh = Some(merge_dynamic_context_refresh(
            effects.dynamic_context_refresh,
            refresh,
        ));
    }
    if !dispatch.is_error {
        match dispatch.output.continuation {
            ToolContinuation::Continue => {}
            ToolContinuation::YieldUntilInput => effects.yield_until_input = true,
            ToolContinuation::WaitingForHuman => {
                effects.waiting_for_human = Some(
                    dispatch
                        .output
                        .metadata
                        .get(METADATA_HUMAN_REQUEST_ID_KEY)
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| TurnError::MissingHumanRequestId {
                            tool: call.name.clone(),
                        })?
                        .to_owned(),
                );
            }
        }
    }
    if let Some(recovery) = dispatch.recovery.clone() {
        unresolved.insert(recovery.tool.clone(), recovery);
    } else {
        unresolved.remove(&call.name);
    }
    Ok(())
}

impl ToolStepCheckpoint {
    pub fn validate(&self, session_id: &str, turn_id: &str) -> Result<(), TurnError> {
        if self.step == 0
            || self.step == u32::MAX
            || self.assistant_id != assistant_message_id(turn_id, self.step)
            || self.calls.is_empty()
            || self.calls.len() != self.call_positions.len()
            || self.next_call > self.calls.len()
            || self
                .call_positions
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
            || self.orchestration_snapshot.turn_id != turn_id
            || self.orchestration_snapshot.step != self.step
        {
            return Err(crate::state::TurnStateError::InvalidData.into());
        }
        let request = RunTurnRequest::new(session_id, turn_id, DynamicContext::new(""));
        let mut indices = BTreeSet::new();
        let mut waits = BTreeSet::new();
        for pending in &self.pending {
            if pending.index >= self.next_call
                || !indices.insert(pending.index)
                || !waits.insert(&pending.reference.id)
            {
                return Err(crate::state::TurnStateError::InvalidData.into());
            }
            crate::wait::validate_binding(
                &pending.reference,
                &request,
                &self.calls[pending.index],
            )?;
        }
        Ok(())
    }

    /// Protect only an exact, complete set of unfinished invocations. A dangling
    /// row from another advance or a forged wait marker never suppresses repair.
    pub fn covers_unfinished(
        &self,
        session_id: &str,
        turn_id: &str,
        parts: &[PartRecord],
        registered: bool,
    ) -> Result<bool, TurnError> {
        self.validate(session_id, turn_id)?;
        let pending: BTreeMap<_, _> = self
            .pending
            .iter()
            .map(|pending| (pending.index, &pending.reference))
            .collect();
        let expected: BTreeMap<_, _> = self
            .calls
            .iter()
            .enumerate()
            .filter(|(index, _)| *index >= self.next_call || pending.contains_key(index))
            .map(|(index, call)| {
                (
                    positional_part_id(
                        turn_id,
                        self.step,
                        self.call_positions[index],
                        PART_KIND_TOOL,
                    ),
                    (index, call),
                )
            })
            .collect();
        if parts.len() != expected.len() {
            return Ok(false);
        }
        let mut seen = BTreeSet::new();
        for part in parts {
            let Some((index, call)) = expected.get(&part.id) else {
                return Ok(false);
            };
            let state = part.data.get("state");
            if !seen.insert(&part.id)
                || part.session_id != session_id
                || part.message_id != self.assistant_id
                || part.kind != PartKind::Tool
                || part.time_created != self.assistant_time_created
                || part.data.get("callID").and_then(Value::as_str) != Some(call.id.as_str())
                || part.data.get("tool").and_then(Value::as_str) != Some(call.name.as_str())
                || state.and_then(|state| state.get("input")) != Some(&call.input)
                || state
                    .and_then(|state| state.get("status"))
                    .and_then(Value::as_str)
                    != Some("pending")
                || state.is_some_and(|state| state.get(DISPATCH_STARTED_FIELD).is_some())
            {
                return Ok(false);
            }
            let marker = state.and_then(|state| state.get("waitRef"));
            match pending.get(index) {
                Some(reference) => {
                    let expected = json!(reference);
                    if marker != Some(&expected) && (registered || marker.is_some()) {
                        return Ok(false);
                    }
                }
                None if marker.is_some() => return Ok(false),
                None => {}
            }
        }
        Ok(true)
    }

    pub fn waiting_parts(&self, parts: &[PartRecord]) -> Result<Vec<PartRecord>, TurnError> {
        self.pending
            .iter()
            .map(|pending| {
                let call = self
                    .calls
                    .get(pending.index)
                    .ok_or(crate::state::TurnStateError::InvalidData)?;
                let mut part = parts
                    .iter()
                    .find(|part| {
                        part.data.get("callID").and_then(Value::as_str) == Some(call.id.as_str())
                    })
                    .cloned()
                    .ok_or(crate::state::TurnStateError::Conflict)?;
                let state = part
                    .data
                    .get_mut("state")
                    .and_then(Value::as_object_mut)
                    .ok_or(crate::state::TurnStateError::InvalidData)?;
                state.insert("waitRef".to_owned(), json!(pending.reference));
                Ok(part)
            })
            .collect()
    }
}

/// Compute ordered tool results and their bookkeeping without performing I/O.
/// Both storage providers commit these values with the next checkpoint.
pub fn consume(
    request: &RunTurnRequest,
    checkpoint: &mut crate::advance::LoopCheckpoint,
    completions: &[crate::wait::WaitCompletion],
) -> Result<Vec<PartRecord>, TurnError> {
    let phase = checkpoint
        .tool_step
        .as_mut()
        .ok_or(crate::state::TurnStateError::Conflict)?;
    phase.validate(&request.session_id, &request.turn_id)?;
    if phase.pending.is_empty() || completions.len() != phase.pending.len() {
        return Err(crate::state::TurnStateError::Conflict.into());
    }
    let mut parts = Vec::with_capacity(completions.len());
    let mut completion_ids = BTreeSet::new();
    for pending in &phase.pending {
        let completion = completions
            .iter()
            .find(|completion| completion.reference == pending.reference)
            .ok_or(crate::state::TurnStateError::Conflict)?;
        completion.validate()?;
        if !completion_ids.insert(&completion.id) {
            return Err(crate::state::TurnStateError::Conflict.into());
        }
        let call = &phase.calls[pending.index];
        let display_name = tool_display_name(&phase.locked_tools, &call.name);
        let mut part = tool_result_part(
            request,
            ToolPartIdentity {
                step: phase.step,
                position: phase.call_positions[pending.index],
                message_time_created: phase.assistant_time_created,
                message_id: &phase.assistant_id,
                call,
                display_name: &display_name,
                ui_intent: tool_ui_intent(&phase.locked_tools, &call.name),
                schema_identity: tool_schema_identity(&phase.locked_tools, &call.name),
            },
            &completion.result,
        )?;
        part.data
            .insert("completionID".to_owned(), json!(completion.id));
        apply_result(
            &mut phase.effects,
            &mut checkpoint.unresolved_tool_failures,
            call,
            &completion.result,
            phase.calls.len() == 1,
        )?;
        parts.push(part);
    }
    checkpoint.tool_calls_dispatched = checkpoint
        .tool_calls_dispatched
        .saturating_add(u32::try_from(completions.len()).unwrap_or(u32::MAX));
    phase.pending.clear();
    Ok(parts)
}

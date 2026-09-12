//! Exercise report admission through the native host, durable inbox and provider.

use super::*;
use zuno_db::inbox::SessionInput;
use zuno_db::session_execution::SessionExecutionStore;
use zuno_db::session_work_cycle::SessionWorkCycle;
use zuno_types::execution::SessionScheduling;

fn activate_input(host: &mut TurnHost, id: &str) -> SessionWorkCycle {
    let (message, parts) = host
        .prepare_turn_user_message("Observe the requested background work.", Some(id), None)
        .expect("prepare ordinary input");
    host.persist_user_input(&message, &parts)
        .expect("native input promotion");
    zuno_db::session_work_cycle::current_in(&host.connection, &host.session_id)
        .expect("read cycle")
        .expect("ordinary input owns a cycle")
}

fn set_readiness(host: &TurnHost, readiness: SessionReadiness) {
    let state = execution(host);
    SessionExecutionStore::new(Arc::clone(&host.database))
        .set_scheduling(
            &host.session_id,
            state.revision,
            SessionScheduling {
                readiness,
                ..state.scheduling.unwrap_or_default()
            },
            zuno_db::message::now_millis(),
        )
        .expect("fixture scheduling transition");
}

fn promote_report(host: &TurnHost, cycle_id: &str, id: &str) -> SessionInput {
    let now = zuno_db::message::now_millis();
    let source_key = format!("background:{id}:1");
    let payload = json!({
        "kind": "backgroundExecutionReport",
        "executionID": id,
        "status": "completed",
        "text": format!("REPORT_{id}: the observer completed.")
    });
    let deliveries = CompletionDeliveryStore::new(Arc::clone(&host.database));
    deliveries
        .publish(
            CompletionEnvelope {
                source_key: source_key.clone(),
                source: CompletionSource::BackgroundExecution,
                terminal_revision: 1,
                parent_session_id: host.session_id.clone(),
                cycle_id: Some(cycle_id.to_owned()),
                payload: payload.clone(),
            },
            now,
        )
        .expect("publish terminal completion");
    let (receipt, input) = deliveries
        .claim_callback(
            &source_key,
            NewSessionInput::new(id, &host.session_id, payload, InputDelivery::Queue, now)
                .with_source_key(&source_key)
                .with_trigger_kind(InputTriggerKind::Automatic)
                .with_cycle_id(Some(cycle_id)),
            now,
        )
        .expect("claim callback")
        .expect("first callback owns completion");
    assert_eq!(receipt.owner, Some(CompletionOwner::Callback));
    host.inbox
        .promote_id(&host.session_id, &input.id)
        .expect("promote callback")
        .expect("current authorized report is promotable")
}

async fn drive_reports(host: &mut TurnHost, inputs: &[SessionInput]) -> Vec<TurnEvent> {
    let reports = ReportBatch::project(inputs);
    assert!(reports.undecodable().is_empty());
    assert_eq!(reports.reports().len(), inputs.len());
    let guard = host
        .runs
        .begin_turn(host.session_id.clone())
        .expect("report turn lease");
    let (sender, receiver) = zuno_engine::r#loop::event_channel();
    let (result, events) = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        tokio::join!(
            host.drive_promoted_reports_with_guard(reports.reports(), &guard, sender),
            collect_turn_events(receiver)
        )
    })
    .await
    .expect("report batch terminates");
    result.expect("report drive");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, TurnEvent::TurnFailed { .. })),
        "{events:#?}"
    );
    events
}

fn assert_reports_consumed_once(host: &TurnHost, inputs: &[SessionInput]) {
    let deliveries = CompletionDeliveryStore::new(Arc::clone(&host.database));
    for input in inputs {
        assert_eq!(
            host.inbox
                .get(&host.session_id, &input.id)
                .unwrap()
                .unwrap()
                .state,
            SubmissionState::Consumed
        );
        let count: i64 = host
            .connection
            .query_row(
                "SELECT count(*) FROM message WHERE session_id=?1 AND id=?2",
                (&host.session_id, &input.id),
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "each report has exactly one durable message");
        let receipt = deliveries
            .get(input.source_key.as_deref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(receipt.owner, Some(CompletionOwner::Callback));
        assert_eq!(receipt.input_id.as_deref(), Some(input.id.as_str()));
        assert_eq!(receipt.envelope.cycle_id, input.cycle_id);
        assert!(
            deliveries
                .claim_inline(
                    input.source_key.as_deref().unwrap(),
                    zuno_db::message::now_millis()
                )
                .unwrap()
                .is_none(),
            "an inline read cannot reclaim the consumed callback"
        );
    }
}

fn assert_deferred(events: &[TurnEvent]) {
    assert!(
        events.iter().any(|event| matches!(
            event,
            TurnEvent::Notice { code, detail, .. }
                if code == "report_deferred_by_execution_state"
                    && detail.contains("current work cycle")
                    && !detail.contains("Goal is resumed")
        )),
        "{events:#?}"
    );
}

async fn independent_report_batch(goal_status: Option<GoalStatus>) {
    let response = vec![
        StreamEvent::TextDelta("Both observer results are accounted for.".to_owned()),
        StreamEvent::TokenUsage {
            input_tokens: Some(2),
            output_tokens: Some(1),
            reasoning_tokens: None,
            cache_read_input_tokens: None,
            cache_write_input_tokens: None,
            accounting: zuno_llm::event::PromptAccounting::CacheInsideInput,
        },
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ];
    let (_directory, mut host, provider, _) = mock_provider_host("build", vec![response]).await;
    if let Some(status) = goal_status {
        let goal = host
            .goal_store
            .create_goal(&host.session_id, "Unrelated historical objective", Some(1))
            .unwrap();
        match status {
            GoalStatus::Complete => {
                host.goal_store
                    .complete_checked(&host.session_id, goal.revision)
                    .unwrap();
            }
            GoalStatus::Paused => {
                host.goal_store
                    .pause_with_reason(
                        &host.session_id,
                        zuno_goal::GoalPauseReason::UserInterruption,
                    )
                    .unwrap();
            }
            GoalStatus::BudgetLimited => {
                host.goal_store
                    .record_usage(&host.session_id, 1, 0, true)
                    .unwrap();
            }
            _ => unreachable!("only historical Goal states belong in this fixture"),
        }
    }
    let goal_before = host.goal_store.goal(&host.session_id).unwrap();
    let cycle = activate_input(&mut host, "independent-request");
    assert_eq!(cycle.goal_id, None);
    let inputs = [
        promote_report(&host, &cycle.cycle_id, "report-first"),
        promote_report(&host, &cycle.cycle_id, "report-second"),
    ];
    let events = drive_reports(&mut host, &inputs).await;
    assert_eq!(
        provider.calls(),
        1,
        "an ordinary cycle's callbacks must not inherit {goal_status:?}: {events:#?}"
    );
    assert_eq!(completed_events(&events), 1);
    assert_eq!(turn_starts(&host), 1);
    assert_reports_consumed_once(&host, &inputs);
    let request = serde_json::to_string(&provider.requests.lock().unwrap()[0].messages).unwrap();
    for input in &inputs {
        assert!(request.contains(&format!("REPORT_{}", input.id)));
    }
    assert_eq!(
        serde_json::to_value(host.goal_store.goal(&host.session_id).unwrap()).unwrap(),
        serde_json::to_value(goal_before).unwrap(),
        "ordinary report execution must neither resume nor charge the historical Goal"
    );
    drive_reports(&mut host, &inputs).await;
    assert_eq!(provider.calls(), 1, "redelivery must not sample again");
    assert_reports_consumed_once(&host, &inputs);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn ordinary_report_batch_without_a_goal_invokes_provider_once() {
    independent_report_batch(None).await;
}

#[tokio::test]
async fn independent_report_batch_with_paused_goal_invokes_provider_once() {
    independent_report_batch(Some(GoalStatus::Paused)).await;
}

#[tokio::test]
async fn independent_report_batch_with_completed_goal_invokes_provider_once() {
    independent_report_batch(Some(GoalStatus::Complete)).await;
}

#[tokio::test]
async fn completed_foreground_background_report_with_historical_goal_continues_same_cycle_once() {
    let (_directory, mut host, provider, work) = mock_provider_host(
        "build",
        vec![
            final_response("Foreground work is finished; the observer owns its result."),
            final_response("The background observation completed successfully."),
        ],
    )
    .await;
    let historical = host
        .goal_store
        .create_goal(
            &host.session_id,
            "An already finished unrelated objective",
            None,
        )
        .unwrap();
    host.goal_store
        .complete_checked(&host.session_id, historical.revision)
        .unwrap();
    let historical = host.goal_store.goal(&host.session_id).unwrap();
    let foreground = drive_user(&mut host, "Observe the requested background work.").await;
    assert_eq!(provider.calls(), 1);
    assert_eq!(completed_events(&foreground), 1);
    assert_eq!(final_messages(&host), 1);
    let mut cycle = zuno_db::session_work_cycle::current_in(&host.connection, &host.session_id)
        .unwrap()
        .unwrap();
    assert_eq!(cycle.goal_id, None);
    assert_eq!(cycle.stopped, None);

    // Restore the anonymized, observed callback boundary after a finished
    // foreground turn: Idle/Ready, a bound Plan, and explicit Goal independence.
    // This is a durable-state fixture, not a replay of an external CI observer.
    let plan = seed_scripted_plan(&work, &host.session_id, false)
        .plan
        .unwrap();
    // The report turn settles its adopted Plan before finalizing. One callback
    // owns one turn, which may include both a tool request and its final answer.
    provider.scripts.lock().unwrap().push_front(vec![
        StreamEvent::ToolUseStart {
            id: "settle-observer-plan".to_owned(),
            name: "plan_update".to_owned(),
        },
        StreamEvent::ToolInputDelta {
            id: "settle-observer-plan".to_owned(),
            delta: json!({
                "action": "patch", "expected_revision": plan.revision,
                "steps": [{"id": plan.steps[0].id, "status": "completed"}]
            })
            .to_string(),
        },
        StreamEvent::ToolUseEnd {
            id: "settle-observer-plan".to_owned(),
        },
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::ToolCalls),
        },
    ]);
    cycle.plan_id = Some(plan.id.clone());
    host.database
        .transaction(|tx| {
            zuno_db::session_work_cycle::save_in(tx, &cycle, zuno_db::message::now_millis())
        })
        .unwrap();
    set_readiness(&host, SessionReadiness::Ready);
    assert_eq!(execution(&host).phase, SessionExecutionPhase::Idle);
    let explicit_null: String = host
        .connection
        .query_row(
            "SELECT json_type(data,'$.goalId') FROM session_work_cycle
             WHERE session_id=?1 AND cycle_id=?2",
            (&host.session_id, &cycle.cycle_id),
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(explicit_null, "null");
    let provider_events_before: i64 = host
        .connection
        .query_row(
            "SELECT count(DISTINCT json_extract(data,'$.requestID')) FROM event WHERE aggregate_id=?1
             AND type='session.provider.request.1'",
            [&host.session_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(provider_events_before, 1);
    let input = promote_report(&host, &cycle.cycle_id, "report-observer-finished");
    let events = drive_reports(&mut host, std::slice::from_ref(&input)).await;
    assert_eq!(
        provider.calls(),
        3,
        "the callback has one tool request and one final answer in the same turn"
    );
    assert_eq!(completed_events(&events), 1);
    assert_eq!(turn_starts(&host), 2);
    assert_eq!(final_messages(&host), 2);
    assert_eq!(
        execution(&host).cycle_id.as_deref(),
        Some(cycle.cycle_id.as_str())
    );
    let current = zuno_db::session_work_cycle::current_in(&host.connection, &host.session_id)
        .unwrap()
        .unwrap();
    assert_eq!(current.goal_id, None);
    assert_eq!(current.plan_id.as_deref(), Some(plan.id.as_str()));
    assert_eq!(current.stopped, None);
    assert_eq!(
        serde_json::to_value(host.goal_store.goal(&host.session_id).unwrap()).unwrap(),
        serde_json::to_value(historical).unwrap()
    );
    assert_reports_consumed_once(&host, std::slice::from_ref(&input));
    let provider_events_after: i64 = host
        .connection
        .query_row(
            "SELECT count(DISTINCT json_extract(data,'$.requestID')) FROM event WHERE aggregate_id=?1
             AND type='session.provider.request.1'",
            [&host.session_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(provider_events_after - provider_events_before, 2);
    assert!(
        work.plan(&host.session_id)
            .unwrap()
            .unwrap()
            .steps
            .iter()
            .all(|step| step.status.is_terminal())
    );
    drive_reports(&mut host, &[input]).await;
    assert_eq!(provider.calls(), 3, "a consumed callback cannot run again");
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn independent_report_batch_with_budget_limited_goal_invokes_provider_once() {
    independent_report_batch(Some(GoalStatus::BudgetLimited)).await;
}

#[tokio::test]
async fn legacy_report_scope_cannot_bypass_an_inactive_goal() {
    let (_directory, mut host, provider, _) = mock_provider_host(
        "build",
        vec![final_response("Unexpected legacy continuation.")],
    )
    .await;
    host.goal_store
        .create_goal(
            &host.session_id,
            "Historical objective with unknown legacy ownership",
            None,
        )
        .unwrap();
    host.goal_store
        .pause_with_reason(
            &host.session_id,
            zuno_goal::GoalPauseReason::UserInterruption,
        )
        .unwrap();
    let cycle = activate_input(&mut host, "legacy-request");
    let input = promote_report(&host, &cycle.cycle_id, "report-legacy");
    host.connection
        .execute(
            "DELETE FROM session_work_cycle WHERE session_id=?1 AND cycle_id=?2",
            (&host.session_id, &cycle.cycle_id),
        )
        .unwrap();
    let before = execution(&host);
    assert_eq!(before.cycle_id.as_deref(), Some(cycle.cycle_id.as_str()));
    assert!(
        zuno_db::session_work_cycle::current_in(&host.connection, &host.session_id)
            .unwrap()
            .is_none()
    );
    let events = drive_reports(&mut host, std::slice::from_ref(&input)).await;
    assert_eq!(
        provider.calls(),
        0,
        "unknown ownership is not an independent cycle"
    );
    assert_deferred(&events);
    assert_eq!(execution(&host), before);
    assert_reports_consumed_once(&host, &[input]);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn legacy_live_application_cannot_bypass_an_inactive_goal() {
    let (_directory, mut host, provider, _) = mock_provider_host("build", Vec::new()).await;
    host.goal_store
        .create_goal(&host.session_id, "Unknown legacy ownership", None)
        .unwrap();
    host.goal_store
        .pause_with_reason(
            &host.session_id,
            zuno_goal::GoalPauseReason::UserInterruption,
        )
        .unwrap();
    let cycle = activate_input(&mut host, "legacy-live-request");
    let input = promote_report(&host, &cycle.cycle_id, "report-legacy-live");
    host.connection
        .execute(
            "DELETE FROM session_work_cycle WHERE session_id=?1 AND cycle_id=?2",
            (&host.session_id, &cycle.cycle_id),
        )
        .unwrap();
    let before = execution(&host);
    for claimed in [None, Some(&input)] {
        let admission = host
            .database
            .transaction(|tx| {
                zuno_db::session_wake::model_application_admission_in(tx, &host.session_id, claimed)
            })
            .unwrap();
        assert_eq!(
            admission,
            WakeAdmission::Reject,
            "legacy claimed={claimed:?}"
        );
    }
    assert_eq!(provider.calls(), 0);
    assert_eq!(execution(&host), before);
    assert_eq!(
        host.inbox
            .get(&host.session_id, &input.id)
            .unwrap()
            .unwrap()
            .state,
        SubmissionState::Queued,
        "a refused live claim returns to its admitted lane"
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn mixed_report_batch_uses_the_eligible_report_as_its_planning_seed() {
    let (_directory, mut host, provider, _) = mock_provider_host(
        "build",
        vec![final_response("The current observer completed.")],
    )
    .await;
    let old_cycle = activate_input(&mut host, "mixed-old-request");
    let mut old = promote_report(&host, &old_cycle.cycle_id, "report-mixed-old");
    let current_cycle = activate_input(&mut host, "mixed-current-request");
    let current = promote_report(&host, &current_cycle.cycle_id, "report-mixed-current");
    // Model a late old-cycle report that is newest in the projected batch.
    // Its promotion is already held, but its cycle no longer authorizes a wake.
    old.time_created = current.time_created + 1;
    host.connection
        .execute(
            "UPDATE session_input SET time_created=?1 WHERE session_id=?2 AND id=?3",
            (old.time_created, &host.session_id, &old.id),
        )
        .unwrap();
    let inputs = [current.clone(), old.clone()];
    let reports = ReportBatch::project(&inputs);
    assert_eq!(
        reports
            .reports()
            .iter()
            .find(|report| report.newest)
            .unwrap()
            .input_id,
        old.id
    );
    let before = execution(&host);
    let (admitted, planning) = host
        .database
        .transaction(|tx| {
            admit_report_continuation_in(tx, &host.session_id, reports.reports(), &inputs)
        })
        .unwrap()
        .unwrap();
    assert_eq!(admitted.id, current.id);
    assert_eq!(
        planning.input_id, current.id,
        "a rejected old cycle must not supply the planning seed"
    );
    let expected = reports
        .reports()
        .iter()
        .find(|report| report.input_id == current.id)
        .unwrap();
    assert_eq!(
        &planning, expected,
        "planning text and source travel together"
    );
    assert_eq!(execution(&host), before);
    let events = drive_reports(&mut host, &inputs).await;
    assert_eq!(provider.calls(), 1);
    assert_eq!(completed_events(&events), 1);
    assert_eq!(
        execution(&host).cycle_id.as_deref(),
        Some(current_cycle.cycle_id.as_str())
    );
    assert_reports_consumed_once(&host, &inputs);
    let request = serde_json::to_string(&provider.requests.lock().unwrap()[0].messages).unwrap();
    assert!(
        request.contains("REPORT_report-mixed-old"),
        "old facts remain history"
    );
    assert!(request.contains("REPORT_report-mixed-current"));
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn promoted_report_rechecks_its_bound_goal_before_provider_execution() {
    let (_directory, mut host, provider, _) = mock_provider_host("build", Vec::new()).await;
    let goal = host
        .goal_store
        .create_goal(&host.session_id, "The current objective", None)
        .unwrap();
    let cycle = activate_input(&mut host, "goal-request");
    assert_eq!(cycle.goal_id.as_deref(), Some(goal.goal_id.as_str()));
    let input = promote_report(&host, &cycle.cycle_id, "report-goal");
    // A native Goal pause can happen after promotion but before report application.
    host.goal_store
        .pause_with_reason(
            &host.session_id,
            zuno_goal::GoalPauseReason::UserInterruption,
        )
        .unwrap();
    let before = execution(&host);
    let goal_before = host.goal_store.goal(&host.session_id).unwrap();
    let events = drive_reports(&mut host, std::slice::from_ref(&input)).await;
    assert_deferred(&events);
    assert_eq!(provider.calls(), 0);
    assert_eq!(turn_starts(&host), 0);
    assert_eq!(execution(&host), before);
    assert_eq!(
        serde_json::to_value(host.goal_store.goal(&host.session_id).unwrap()).unwrap(),
        serde_json::to_value(goal_before).unwrap()
    );
    assert_reports_consumed_once(&host, &[input]);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn promoted_report_preserves_human_budget_auth_uncertain_and_stop_gates() {
    let gates = [
        SessionReadiness::WaitingHuman {
            request_id: "approval-exact".to_owned(),
        },
        SessionReadiness::Paused {
            reason: SessionPauseReason::TurnBudget,
        },
        SessionReadiness::Paused {
            reason: SessionPauseReason::Authentication,
        },
        SessionReadiness::Paused {
            reason: SessionPauseReason::UncertainSideEffect,
        },
        SessionReadiness::Paused {
            reason: SessionPauseReason::User,
        },
        SessionReadiness::Paused {
            reason: SessionPauseReason::Blocked,
        },
        SessionReadiness::Paused {
            reason: SessionPauseReason::NoProgress,
        },
        SessionReadiness::Paused {
            reason: SessionPauseReason::NoExecutableWork,
        },
        SessionReadiness::WaitingExternal {
            source_id: "another-observer".to_owned(),
            origin_cycle_id: "another-cycle".to_owned(),
        },
        SessionReadiness::Completed,
        SessionReadiness::Ready, // Independently test the cycle's stop marker.
    ];
    for readiness in gates {
        let (_directory, mut host, provider, _) = mock_provider_host("build", Vec::new()).await;
        let cycle = activate_input(&mut host, "gated-request");
        let input = promote_report(&host, &cycle.cycle_id, "report-gated");
        let humans = zuno_db::human_request::HumanRequestStore::new(Arc::clone(&host.database));
        let approval = if let SessionReadiness::WaitingHuman { request_id } = &readiness {
            Some(
                humans
                    .create(zuno_db::human_request::NewHumanRequest {
                        id: request_id.clone(),
                        session_id: host.session_id.clone(),
                        goal_id: None,
                        kind: zuno_db::human_request::HumanRequestKind::Permission,
                        payload: json!({"permission": "write", "pattern": "protected.txt"}),
                        message_id: None,
                        call_id: None,
                        time_created: zuno_db::message::now_millis(),
                    })
                    .unwrap(),
            )
        } else {
            None
        };
        if readiness == SessionReadiness::Ready {
            host.session_control
                .begin_engine_turn(&host.session_id, &cycle.cycle_id, "interrupted-turn")
                .unwrap();
            host.session_control
                .stop_turn(
                    &host.session_id,
                    &cycle.cycle_id,
                    "interrupted-turn",
                    true,
                    zuno_db::message::now_millis(),
                )
                .unwrap();
        } else {
            set_readiness(&host, readiness);
        }
        let before = execution(&host);
        let scope_before =
            zuno_db::session_work_cycle::current_in(&host.connection, &host.session_id).unwrap();
        let events = drive_reports(&mut host, std::slice::from_ref(&input)).await;
        assert_deferred(&events);
        assert_eq!(provider.calls(), 0);
        assert_eq!(turn_starts(&host), 0);
        assert_eq!(execution(&host), before);
        assert_eq!(
            zuno_db::session_work_cycle::current_in(&host.connection, &host.session_id).unwrap(),
            scope_before
        );
        if let Some(approval) = approval {
            assert_eq!(humans.get(&approval.id).unwrap().as_ref(), Some(&approval));
        }
        assert_reports_consumed_once(&host, &[input]);
        host.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn report_rechecks_a_stop_committed_during_history_persistence() {
    let (_directory, mut host, provider, _) = mock_provider_host("build", Vec::new()).await;
    let cycle = activate_input(&mut host, "racing-request");
    let input = promote_report(&host, &cycle.cycle_id, "report-racing");
    // Deterministically put a stop between the old preflight check and wake
    // application, without timing a second task or changing a real session.
    host.connection
        .execute_batch(
            "CREATE TEMP TRIGGER stop_during_report_commit AFTER UPDATE OF state ON session_input
             WHEN NEW.id='report-racing' AND NEW.state='consumed'
             BEGIN
               UPDATE session_work_cycle SET data=json_set(data,'$.stopped',
                 json('{\"userCancelled\":true,\"atMs\":1}'))
               WHERE session_id=NEW.session_id AND cycle_id=NEW.cycle_id;
             END;",
        )
        .unwrap();
    let before = execution(&host);
    let events = drive_reports(&mut host, std::slice::from_ref(&input)).await;
    assert_deferred(&events);
    assert_eq!(provider.calls(), 0);
    assert_eq!(turn_starts(&host), 0);
    assert_eq!(execution(&host), before, "no continuation may be created");
    assert!(
        zuno_db::session_work_cycle::is_stopped_in(
            &host.connection,
            &host.session_id,
            &cycle.cycle_id
        )
        .unwrap()
    );
    assert_reports_consumed_once(&host, &[input]);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn old_cycle_report_cannot_wake_an_independent_new_cycle() {
    let (_directory, mut host, provider, _) = mock_provider_host("build", Vec::new()).await;
    let old = activate_input(&mut host, "old-request");
    let input = promote_report(&host, &old.cycle_id, "report-old");
    let current = activate_input(&mut host, "new-request");
    assert_ne!(old.cycle_id, current.cycle_id);
    let before = execution(&host);
    let events = drive_reports(&mut host, std::slice::from_ref(&input)).await;
    assert_deferred(&events);
    assert_eq!(provider.calls(), 0);
    assert_eq!(execution(&host), before);
    assert_reports_consumed_once(&host, &[input]);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn native_goal_resume_alias_delivers_original_report_once() {
    let (_directory, mut host, provider, _) = mock_provider_host(
        "build",
        vec![final_response("The resumed observer completed.")],
    )
    .await;
    host.goal_store
        .create_goal(&host.session_id, "Observe the requested work", None)
        .unwrap();
    let old = activate_input(&mut host, "resumed-goal-request");
    let input = promote_report(&host, &old.cycle_id, "report-resumed");
    host.session_control
        .begin_engine_turn(&host.session_id, &old.cycle_id, "stopped-goal-turn")
        .unwrap();
    host.session_control
        .stop_turn(
            &host.session_id,
            &old.cycle_id,
            "stopped-goal-turn",
            true,
            zuno_db::message::now_millis(),
        )
        .unwrap();
    let paused = host
        .goal_store
        .pause_with_reason(
            &host.session_id,
            zuno_goal::GoalPauseReason::UserInterruption,
        )
        .unwrap()
        .unwrap();
    // Preserve the exact observer wait during native resume. The fresh cycle
    // authorizes delivery of the old report without granting the old cycle a turn.
    set_readiness(
        &host,
        SessionReadiness::WaitingExternal {
            source_id: "report-resumed".to_owned(),
            origin_cycle_id: old.cycle_id.clone(),
        },
    );
    let resumed = host
        .session_control
        .resume_goal(
            &zuno_types::goal_resume::GoalResumeRequest {
                session_id: host.session_id.clone(),
                goal_id: paused.goal_id,
                expected_revision: paused.revision,
                input_id: None,
            },
            zuno_db::message::now_millis(),
        )
        .unwrap();
    assert!(
        resumed.input.is_none(),
        "only the exact completion satisfies the wait"
    );
    assert_ne!(
        resumed.state.cycle_id.as_deref(),
        Some(old.cycle_id.as_str())
    );
    let events = drive_reports(&mut host, std::slice::from_ref(&input)).await;
    assert_eq!(provider.calls(), 1);
    assert_eq!(completed_events(&events), 1);
    assert_eq!(execution(&host).cycle_id, resumed.state.cycle_id);
    assert!(
        zuno_db::session_work_cycle::is_stopped_in(
            &host.connection,
            &host.session_id,
            &old.cycle_id
        )
        .unwrap()
    );
    assert_reports_consumed_once(&host, std::slice::from_ref(&input));
    drive_reports(&mut host, &[input]).await;
    assert_eq!(provider.calls(), 1);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn exact_external_wait_report_resumes_only_that_wait() {
    let (_directory, mut host, provider, _) =
        mock_provider_host("build", vec![final_response("The observed work is done.")]).await;
    let cycle = activate_input(&mut host, "waiting-request");
    set_readiness(
        &host,
        SessionReadiness::WaitingExternal {
            source_id: "report-waited".to_owned(),
            origin_cycle_id: cycle.cycle_id.clone(),
        },
    );
    let input = promote_report(&host, &cycle.cycle_id, "report-waited");
    let events = drive_reports(&mut host, std::slice::from_ref(&input)).await;
    assert_eq!(provider.calls(), 1);
    assert_eq!(completed_events(&events), 1);
    assert_eq!(
        execution(&host).cycle_id.as_deref(),
        Some(cycle.cycle_id.as_str())
    );
    assert_reports_consumed_once(&host, &[input]);
    host.shutdown().await.unwrap();
}

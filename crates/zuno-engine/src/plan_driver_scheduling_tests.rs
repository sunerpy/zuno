use super::tests::{pool, unfinished};
use super::*;
use zuno_types::execution::{ContinuationToken, SessionScheduling, TurnExecutionIdentity};

fn state(pool: &Arc<Pool>) -> SessionExecutionState {
    execution_in(&pool.get().expect("connection"), "ses").expect("execution state")
}

fn authorized(pool: &Arc<Pool>) -> SessionExecutionState {
    let mut current = state(pool);
    let identity =
        TurnExecutionIdentity::new("build", "provider", "model").with_reasoning(Some("high"));
    current.work_identity = Some(identity.clone());
    current.authorized_plan_id = Some("plan-1".to_owned());
    current.authorized_plan_revision = Some(4);
    current.handoff_plan_id = Some("plan-1".to_owned());
    current.handoff_plan_revision = Some(4);
    current.cycle_id = Some("origin-cycle".to_owned());
    current.phase = SessionExecutionPhase::Running;
    current.continuation = Some(ContinuationToken {
        cycle_id: "origin-cycle".to_owned(),
        identity,
        mode: CollaborationMode::Work,
        plan_id: Some("plan-1".to_owned()),
        plan_revision: Some(4),
        context_epoch: 7,
        anchor_message_id: Some("anchor-message".to_owned()),
    });
    pool.transaction(|tx| session_execution::update_in(tx, current.revision, current))
        .expect("authority")
}

fn assert_authority(before: &SessionExecutionState, after: &SessionExecutionState) {
    let mut expected = before.clone();
    expected.phase = after.phase;
    expected.revision = after.revision;
    expected.scheduling = after.scheduling.clone();
    expected.time_updated = after.time_updated;
    assert_eq!(&expected, after);
}

fn reconcile(
    driver: &PlanReconciliationDriver,
    input: &PlanReconciliationInput,
    source: &str,
    at_ms: i64,
) -> PlanReconciliationOutcome {
    driver
        .reconcile_with_progress("ses", "origin-cycle", input, true, source, at_ms)
        .expect("reconcile")
}

fn phase(driver: &PlanReconciliationDriver) -> DriverPhaseProjection {
    driver
        .projection("ses")
        .expect("projection")
        .expect("driver phase")
}

fn wait(pool: &Arc<Pool>, reference: SessionWaitReference) -> SessionExecutionState {
    let current = state(pool);
    pool.transaction(|tx| {
        session_execution::set_waiting_in(tx, "ses", current.revision, reference, 50)
    })
    .expect("wait")
}

fn human(request_id: &str) -> SessionWaitReference {
    SessionWaitReference::Human {
        request_id: request_id.to_owned(),
    }
}

fn external(source_id: &str, cycle_id: &str) -> SessionWaitReference {
    SessionWaitReference::External {
        source_id: source_id.to_owned(),
        origin_cycle_id: cycle_id.to_owned(),
    }
}

fn no_progress_pause(driver: &PlanReconciliationDriver) {
    let input = unfinished();
    for attempt in 1..=2 {
        assert_eq!(
            reconcile(driver, &input, "runnable-revision-1", i64::from(attempt)),
            PlanReconciliationOutcome::Decision(PlanReconciliationDecision::ContinueOrdinary {
                attempt
            })
        );
    }
    assert_eq!(
        reconcile(driver, &input, "runnable-revision-1", 3),
        PlanReconciliationOutcome::Paused {
            reason: PlanPauseReason::NoProgress
        }
    );
}

#[test]
fn no_goal_plan_only_and_blocked_todo_do_not_authorize_automatic_recovery() {
    for blocked_todo_only in [false, true] {
        let pool = pool();
        let before = authorized(&pool);
        let driver = PlanReconciliationDriver::new(Arc::clone(&pool));
        let mut input = unfinished();
        input.executable_work = false;
        input.plan_exists = !blocked_todo_only;
        input.active_todo = blocked_todo_only;
        assert_eq!(
            reconcile(&driver, &input, "remaining-work", 10),
            PlanReconciliationOutcome::Paused {
                reason: PlanPauseReason::NoExecutableWork
            }
        );
        let paused = state(&pool);
        assert_authority(&before, &paused);
        assert_eq!(paused.phase, SessionExecutionPhase::Paused);
        assert_eq!(
            paused.scheduling.as_ref().expect("scheduling").readiness,
            SessionReadiness::Paused {
                reason: PlanPauseReason::NoExecutableWork
            }
        );
        assert_eq!(phase(&driver).unchanged_progress_count, 1);
        assert_eq!(
            paused
                .scheduling
                .as_ref()
                .expect("scheduling")
                .progress_fingerprint,
            phase(&driver).progress_fingerprint
        );
        let connection = pool.get().expect("connection");
        let goals: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name='goal'",
                [],
                |row| row.get(0),
            )
            .expect("no Goal table");
        let requests: i64 = connection
            .query_row("SELECT count(*) FROM human_request", [], |row| row.get(0))
            .expect("no manufactured request");
        let messages: i64 = connection
            .query_row("SELECT count(*) FROM message", [], |row| row.get(0))
            .expect("no synthetic reply");
        assert_eq!((goals, requests, messages), (0, 0, 0));
    }
}

#[test]
fn authorized_queued_work_can_run_without_a_plan_todo_or_job() {
    let pool = pool();
    authorized(&pool);
    let driver = PlanReconciliationDriver::new(pool);
    let mut input = unfinished();
    input.plan_exists = false;
    assert!(input.executable_work && !input.active_todo && !input.active_job);
    assert_eq!(
        reconcile(&driver, &input, "authorized-queue-revision-1", 10),
        PlanReconciliationOutcome::Decision(PlanReconciliationDecision::ContinueOrdinary {
            attempt: 1
        })
    );
}

#[test]
fn paused_callback_recovery_and_user_query_preserve_the_durable_cycle_and_streak() {
    let pool = pool();
    let before = authorized(&pool);
    let driver = PlanReconciliationDriver::new(Arc::clone(&pool));
    no_progress_pause(&driver);
    let paused = state(&pool);
    let projection = phase(&driver);
    assert_authority(&before, &paused);
    assert_eq!(
        paused
            .scheduling
            .as_ref()
            .expect("scheduling")
            .unchanged_progress_count,
        3
    );
    let restarted = PlanReconciliationDriver::new(Arc::clone(&pool));
    for wake in [
        SessionWakeSignal::Automatic,
        SessionWakeSignal::Recovery,
        SessionWakeSignal::Callback,
        SessionWakeSignal::ExternalCompletion {
            source_id: "unrelated-callback".to_owned(),
            origin_cycle_id: "new-cycle".to_owned(),
        },
        SessionWakeSignal::UserAnswer {
            request_id: "unrelated-answer".to_owned(),
        },
    ] {
        assert_eq!(
            restarted
                .begin_with_wake("ses", "new-cycle", &wake, 10)
                .expect("admission"),
            None
        );
        assert_eq!(state(&pool), paused);
        assert_eq!(
            phase(&restarted),
            projection,
            "suppression must not append an Executing event"
        );
    }
    assert_eq!(
        restarted
            .begin_with_wake("ses", "query-cycle", &SessionWakeSignal::UserQuery, 11)
            .expect("query"),
        Some("origin-cycle".to_owned())
    );
    assert_eq!(
        reconcile(
            &restarted,
            &unfinished(),
            "changed-snapshot-does-not-resume-a-pause",
            12
        ),
        PlanReconciliationOutcome::Paused {
            reason: PlanPauseReason::NoProgress
        }
    );
    assert!(
        !restarted
            .waiting_retry_for_active_cycle("ses", "retry")
            .expect("retry gate")
    );
    restarted
        .waiting_retry("ses", "retry-cycle", "retry")
        .expect("direct retry gate");
    assert_eq!(state(&pool), paused);
    assert_eq!(phase(&restarted), projection);
}

#[test]
fn callback_and_even_new_user_cycle_ids_alone_never_reset_session_progress() {
    let pool = pool();
    authorized(&pool);
    let driver = PlanReconciliationDriver::new(Arc::clone(&pool));
    assert_eq!(
        reconcile(&driver, &unfinished(), "same-runnable-state", 10),
        PlanReconciliationOutcome::Decision(PlanReconciliationDecision::ContinueOrdinary {
            attempt: 1
        })
    );
    assert_eq!(
        driver
            .begin_with_wake("ses", "callback-cycle", &SessionWakeSignal::Callback, 11)
            .expect("callback"),
        Some("origin-cycle".to_owned())
    );
    assert_eq!(phase(&driver).unchanged_progress_count, 1);
    assert_eq!(
        driver
            .reconcile_with_progress(
                "ses",
                "caller-generated-new-cycle",
                &unfinished(),
                true,
                "same-runnable-state",
                12
            )
            .expect("same runnable state"),
        PlanReconciliationOutcome::Decision(PlanReconciliationDecision::ContinueOrdinary {
            attempt: 2
        })
    );
    assert_eq!(phase(&driver).cycle_id, "origin-cycle");
    assert_eq!(
        driver.begin("ses", "user-cycle").expect("explicit query"),
        "user-cycle"
    );
    assert_eq!(phase(&driver).unchanged_progress_count, 2);
    assert_eq!(
        reconcile(&driver, &unfinished(), "same-runnable-state", 13),
        PlanReconciliationOutcome::Paused {
            reason: PlanPauseReason::NoProgress
        }
    );
    assert_eq!(
        state(&pool)
            .scheduling
            .expect("scheduling")
            .unchanged_progress_count,
        3
    );
}

#[test]
fn runnable_content_change_resets_streak_but_loss_of_runnable_work_pauses() {
    let pool = pool();
    authorized(&pool);
    let driver = PlanReconciliationDriver::new(Arc::clone(&pool));
    let input = unfinished();
    reconcile(&driver, &input, "runnable-revision-1", 10);
    reconcile(&driver, &input, "runnable-revision-1", 11);
    assert_eq!(
        reconcile(&driver, &input, "runnable-revision-2", 12),
        PlanReconciliationOutcome::Decision(PlanReconciliationDecision::ContinueOrdinary {
            attempt: 1
        })
    );
    let previous = state(&pool).scheduling.expect("progress");
    let mut blocked = input;
    blocked.executable_work = false;
    assert_eq!(
        reconcile(&driver, &blocked, "runnable-revision-2", 13),
        PlanReconciliationOutcome::Paused {
            reason: PlanPauseReason::NoExecutableWork
        }
    );
    let current = state(&pool).scheduling.expect("progress");
    assert_ne!(current.progress_fingerprint, previous.progress_fingerprint);
    assert_eq!(current.unchanged_progress_count, 1);
}

#[test]
fn explicit_resume_lifts_the_pause_without_fabricating_progress_or_new_authority() {
    for changed in [false, true] {
        let pool = pool();
        authorized(&pool);
        let driver = PlanReconciliationDriver::new(Arc::clone(&pool));
        no_progress_pause(&driver);
        let paused = state(&pool);
        assert_eq!(
            driver
                .begin_with_wake(
                    "ses",
                    "resume-proposal",
                    &SessionWakeSignal::ExplicitResume,
                    10
                )
                .expect("resume"),
            Some("origin-cycle".to_owned())
        );
        let resumed = state(&pool);
        assert_authority(&paused, &resumed);
        let scheduling = resumed.scheduling.expect("scheduling");
        assert_eq!(scheduling.readiness, SessionReadiness::Ready);
        assert_eq!(scheduling.unchanged_progress_count, 3);
        assert_eq!(
            scheduling.progress_fingerprint,
            paused.scheduling.expect("scheduling").progress_fingerprint
        );
        assert_eq!(phase(&driver).unchanged_progress_count, 3);
        let expected = if changed {
            PlanReconciliationOutcome::Decision(PlanReconciliationDecision::ContinueOrdinary {
                attempt: 1,
            })
        } else {
            PlanReconciliationOutcome::Paused {
                reason: PlanPauseReason::NoProgress,
            }
        };
        assert_eq!(
            reconcile(
                &driver,
                &unfinished(),
                if changed {
                    "runnable-revision-2"
                } else {
                    "runnable-revision-1"
                },
                11
            ),
            expected
        );
    }
}

#[test]
fn ordinary_required_question_waits_and_only_its_matching_answer_resumes() {
    let pool = pool();
    let before = authorized(&pool);
    let driver = PlanReconciliationDriver::new(Arc::clone(&pool));
    reconcile(&driver, &unfinished(), "runnable-revision-1", 10);
    let waiting = wait(&pool, human("required-request"));
    let mut input = unfinished();
    for goal_active in [false, true] {
        input.goal_active = goal_active;
        assert_eq!(
            reconcile(&driver, &input, "new-fingerprint-cannot-clear-wait", 60),
            PlanReconciliationOutcome::Waiting {
                wait: human("required-request")
            }
        );
    }
    let projection = phase(&driver);
    for signal in [
        SessionWakeSignal::Callback,
        SessionWakeSignal::Recovery,
        SessionWakeSignal::Automatic,
        SessionWakeSignal::ExplicitResume,
        SessionWakeSignal::UserAnswer {
            request_id: "other-request".to_owned(),
        },
        SessionWakeSignal::ExternalCompletion {
            source_id: "job-result".to_owned(),
            origin_cycle_id: "origin-cycle".to_owned(),
        },
    ] {
        assert_eq!(
            driver
                .begin_with_wake("ses", "other-cycle", &signal, 61)
                .expect("admission"),
            None
        );
    }
    assert_eq!(state(&pool), waiting);
    assert_eq!(phase(&driver), projection);
    assert_eq!(
        driver
            .begin_with_wake(
                "ses",
                "answer-cycle",
                &SessionWakeSignal::UserAnswer {
                    request_id: "required-request".to_owned(),
                },
                62
            )
            .expect("matching answer"),
        Some("origin-cycle".to_owned())
    );
    assert_authority(&before, &state(&pool));
    assert_eq!(
        state(&pool).scheduling.expect("scheduling").readiness,
        SessionReadiness::Ready
    );
    assert_eq!(phase(&driver).unchanged_progress_count, 1);
}

#[test]
fn awaited_external_completion_matches_both_source_and_origin_cycle() {
    let pool = pool();
    authorized(&pool);
    let driver = PlanReconciliationDriver::new(Arc::clone(&pool));
    let waiting = wait(&pool, external("job-1", "job-origin-cycle"));
    assert_eq!(
        reconcile(&driver, &unfinished(), "runnable", 60),
        PlanReconciliationOutcome::Waiting {
            wait: external("job-1", "job-origin-cycle")
        }
    );
    for (source, cycle) in [("other-job", "job-origin-cycle"), ("job-1", "other-cycle")] {
        assert_eq!(
            driver
                .begin_with_wake(
                    "ses",
                    "callback-proposal",
                    &SessionWakeSignal::ExternalCompletion {
                        source_id: source.to_owned(),
                        origin_cycle_id: cycle.to_owned(),
                    },
                    61
                )
                .expect("unrelated completion"),
            None
        );
        assert_eq!(state(&pool), waiting);
    }
    assert_eq!(
        driver
            .begin_with_wake(
                "ses",
                "callback-proposal",
                &SessionWakeSignal::ExternalCompletion {
                    source_id: "job-1".to_owned(),
                    origin_cycle_id: "job-origin-cycle".to_owned(),
                },
                62
            )
            .expect("awaited completion"),
        Some("origin-cycle".to_owned())
    );
    assert_authority(&waiting, &state(&pool));
    assert_eq!(
        state(&pool).scheduling.expect("scheduling").readiness,
        SessionReadiness::Ready
    );
}

fn plan_mode(pool: &Arc<Pool>) {
    let mut current = state(pool);
    current.mode = CollaborationMode::Plan;
    current.phase = SessionExecutionPhase::Planning;
    current.continuation.as_mut().expect("continuation").mode = CollaborationMode::Plan;
    pool.transaction(|tx| session_execution::update_in(tx, current.revision, current))
        .expect("Plan mode");
}

#[test]
fn exact_plan_authorization_wait_finishes_handoff_without_resuming_work() {
    let pool = pool();
    authorized(&pool);
    plan_mode(&pool);
    let driver = PlanReconciliationDriver::new(Arc::clone(&pool));
    driver.begin("ses", "origin-cycle").expect("Plan turn");
    let waiting = wait(&pool, human("plan-approval"));
    let mut input = unfinished();
    input.planning_handoff = true;
    input.plan_authorization_wait = Some("plan-approval".to_owned());
    input.goal_active = true;
    assert_eq!(
        reconcile(&driver, &input, "completed-plan-revision", 60),
        PlanReconciliationOutcome::Decision(PlanReconciliationDecision::Finish)
    );
    assert_eq!(
        state(&pool),
        waiting,
        "handoff must preserve the approval wait"
    );
    let projection = phase(&driver);
    assert_eq!(projection.phase, DriverPhase::Terminal);
    assert_eq!(projection.reason.as_deref(), Some("planning_handoff_ready"));
    for wake in [
        SessionWakeSignal::Recovery,
        SessionWakeSignal::Callback,
        SessionWakeSignal::ExplicitResume,
    ] {
        assert_eq!(
            driver
                .begin_with_wake("ses", "work-cycle", &wake, 61)
                .expect("gate"),
            None
        );
    }
    assert_eq!(phase(&driver), projection);
    driver
        .begin_with_wake(
            "ses",
            "answer-cycle",
            &SessionWakeSignal::UserAnswer {
                request_id: "plan-approval".to_owned(),
            },
            62,
        )
        .expect("validated answer")
        .expect("admitted");
    assert_eq!(
        state(&pool).mode,
        CollaborationMode::Plan,
        "only the host approval transaction may authorize Work"
    );
    assert_authority(&waiting, &state(&pool));
}

#[test]
fn required_or_replaced_waits_cannot_be_bypassed_by_planning_handoff() {
    for (reference, approval_id) in [
        (human("required-input"), None),
        (
            human("replacement-request"),
            Some("old-plan-approval".to_owned()),
        ),
        (
            external("job-1", "origin-cycle"),
            Some("old-plan-approval".to_owned()),
        ),
    ] {
        let pool = pool();
        authorized(&pool);
        plan_mode(&pool);
        let driver = PlanReconciliationDriver::new(Arc::clone(&pool));
        let waiting = wait(&pool, reference.clone());
        let mut input = unfinished();
        input.planning_handoff = true;
        input.plan_authorization_wait = approval_id;
        assert_eq!(
            reconcile(&driver, &input, "completed-plan-revision", 60),
            PlanReconciliationOutcome::Waiting { wait: reference }
        );
        assert_eq!(state(&pool), waiting);
        assert_ne!(phase(&driver).phase, DriverPhase::Terminal);
    }
}

#[test]
fn an_answer_accepted_before_handoff_still_allows_the_plan_turn_to_finish() {
    let pool = pool();
    authorized(&pool);
    plan_mode(&pool);
    let driver = PlanReconciliationDriver::new(Arc::clone(&pool));
    wait(&pool, human("early-plan-approval"));
    driver
        .begin_with_wake(
            "ses",
            "origin-cycle",
            &SessionWakeSignal::UserAnswer {
                request_id: "early-plan-approval".to_owned(),
            },
            60,
        )
        .expect("early accepted answer")
        .expect("admitted");
    let ready = state(&pool);
    let mut input = unfinished();
    input.planning_handoff = true;
    input.plan_authorization_wait = Some("early-plan-approval".to_owned());
    assert_eq!(
        reconcile(&driver, &input, "completed-plan-revision", 61),
        PlanReconciliationOutcome::Decision(PlanReconciliationDecision::Finish)
    );
    assert_authority(&ready, &state(&pool));
    assert_eq!(
        state(&pool).scheduling.expect("scheduling").readiness,
        SessionReadiness::Completed
    );
    assert_eq!(
        phase(&driver).reason.as_deref(),
        Some("planning_handoff_ready")
    );
}

#[test]
fn completed_new_human_input_starts_a_bound_background_cycle_but_paused_status_does_not() {
    for pre_admitted in [false, true] {
        for signal in [
            SessionWakeSignal::UserQuery,
            SessionWakeSignal::UserAnswer {
                request_id: "validated-deferred-question".to_owned(),
            },
        ] {
            let pool = pool();
            authorized(&pool);
            let driver = PlanReconciliationDriver::new(Arc::clone(&pool));
            reconcile(&driver, &unfinished(), "old-runnable-work", 10);
            let mut finished = unfinished();
            finished.plan_terminal = true;
            finished.executable_work = false;
            assert_eq!(
                reconcile(&driver, &finished, "finished-work", 20),
                PlanReconciliationOutcome::Decision(PlanReconciliationDecision::Finish)
            );
            let completed = state(&pool);
            assert_eq!(completed.phase, SessionExecutionPhase::Completed);
            if pre_admitted {
                assert_eq!(
                    pool.transaction(|tx| session_execution::admit_wake_in(tx, "ses", &signal, 21))
                        .expect("inbox promotion admits the new user input"),
                    WakeAdmission::Resume
                );
            }
            assert_eq!(
                driver
                    .begin_with_wake("ses", "new-human-cycle", &signal, 22)
                    .expect("new human input"),
                Some("new-human-cycle".to_owned())
            );
            let fresh = state(&pool);
            let mut expected = completed.clone();
            expected.cycle_id = Some("new-human-cycle".to_owned());
            expected
                .continuation
                .as_mut()
                .expect("continuation")
                .cycle_id = "new-human-cycle".to_owned();
            assert_authority(&expected, &fresh);
            let scheduling = fresh.scheduling.as_ref().expect("scheduling");
            assert_eq!(scheduling.readiness, SessionReadiness::Ready);
            assert_eq!(
                scheduling.progress_fingerprint,
                completed
                    .scheduling
                    .as_ref()
                    .expect("completed progress")
                    .progress_fingerprint
            );
            assert_eq!(scheduling.unchanged_progress_count, 1);
            assert_eq!(phase(&driver).cycle_id, "new-human-cycle");

            let mut new_work = unfinished();
            new_work.background_wait = Some(external("bg-new-work", "new-human-cycle"));
            assert_eq!(
                reconcile(&driver, &new_work, "background-running", 23),
                PlanReconciliationOutcome::Waiting {
                    wait: external("bg-new-work", "new-human-cycle")
                }
            );
            assert_eq!(state(&pool).cycle_id.as_deref(), Some("new-human-cycle"));
            assert_eq!(
                driver
                    .begin_with_wake(
                        "ses",
                        "callback-proposal",
                        &SessionWakeSignal::ExternalCompletion {
                            source_id: "bg-new-work".to_owned(),
                            origin_cycle_id: "new-human-cycle".to_owned(),
                        },
                        24
                    )
                    .expect("new work's awaited callback"),
                Some("new-human-cycle".to_owned())
            );
            new_work.background_wait = None;
            assert_eq!(
                reconcile(&driver, &new_work, "new-runnable-work", 25),
                PlanReconciliationOutcome::Decision(PlanReconciliationDecision::ContinueOrdinary {
                    attempt: 1
                }),
                "session admission must not claim that a Goal owns continuation"
            );
            assert_eq!(phase(&driver).cycle_id, "new-human-cycle");

            new_work.executable_work = false;
            assert_eq!(
                reconcile(&driver, &new_work, "blocked-new-work", 26),
                PlanReconciliationOutcome::Paused {
                    reason: PlanPauseReason::NoExecutableWork
                }
            );
            let paused = state(&pool);
            let projection = phase(&driver);
            assert_eq!(
                driver
                    .begin_with_wake(
                        "ses",
                        "status-query-cycle",
                        &SessionWakeSignal::UserQuery,
                        27
                    )
                    .expect("status query while paused"),
                Some("new-human-cycle".to_owned())
            );
            assert_eq!(state(&pool), paused);
            assert_eq!(phase(&driver), projection);
        }
    }
}

#[test]
fn progress_pause_and_resume_roll_back_when_the_driver_event_cannot_commit() {
    let pool = pool();
    authorized(&pool);
    let driver = PlanReconciliationDriver::new(Arc::clone(&pool));
    reconcile(&driver, &unfinished(), "runnable-revision-1", 10);
    reconcile(&driver, &unfinished(), "runnable-revision-1", 11);
    let before = state(&pool);
    let projection = phase(&driver);
    pool.get()
        .expect("connection")
        .execute_batch(
            "CREATE TRIGGER reject_driver_event BEFORE INSERT ON event
         WHEN NEW.type='session.driver.phase.1'
         BEGIN SELECT RAISE(ABORT,'event write refused'); END;",
        )
        .expect("event failure");
    assert!(
        driver
            .reconcile_with_progress(
                "ses",
                "origin-cycle",
                &unfinished(),
                true,
                "runnable-revision-1",
                12
            )
            .is_err()
    );
    assert_eq!(state(&pool), before);
    assert_eq!(phase(&driver), projection);
    pool.get()
        .expect("connection")
        .execute_batch("DROP TRIGGER reject_driver_event;")
        .expect("remove test failure");
    reconcile(&driver, &unfinished(), "runnable-revision-1", 13);
    let paused = state(&pool);
    let projection = phase(&driver);
    pool.get()
        .expect("connection")
        .execute_batch(
            "CREATE TRIGGER reject_driver_event BEFORE INSERT ON event
         WHEN NEW.type='session.driver.phase.1'
         BEGIN SELECT RAISE(ABORT,'event write refused'); END;",
        )
        .expect("resume event failure");
    assert!(
        driver
            .begin_with_wake(
                "ses",
                "resume-cycle",
                &SessionWakeSignal::ExplicitResume,
                14
            )
            .is_err()
    );
    assert_eq!(state(&pool), paused);
    assert_eq!(phase(&driver), projection);
}

#[test]
fn a_legacy_callback_cycle_cannot_erase_prior_progress_on_first_admission() {
    let pool = pool();
    authorized(&pool);
    let input = unfinished();
    let fingerprint = stable_progress_fingerprint(&input, "runnable-revision-1");
    pool.transaction(|tx| {
        let mut current = execution_in(tx, "ses")?;
        current.scheduling = None;
        session_execution::update_in(tx, current.revision, current)?;
        record_in(
            tx,
            "ses",
            "older-cycle",
            DriverPhase::Reconciling,
            None,
            Some(&DurableProgress {
                fingerprint: fingerprint.clone(),
                unchanged_count: 2,
            }),
            None,
        )?;
        record_in(
            tx,
            "ses",
            "legacy-callback-cycle",
            DriverPhase::Executing,
            None,
            None,
            None,
        )
    })
    .expect("legacy events without scheduling");
    let driver = PlanReconciliationDriver::new(Arc::clone(&pool));
    assert_eq!(
        driver
            .begin_with_wake(
                "ses",
                "another-callback-cycle",
                &SessionWakeSignal::Callback,
                30
            )
            .expect("callback"),
        Some("origin-cycle".to_owned())
    );
    let scheduling: SessionScheduling = state(&pool).scheduling.expect("backfilled progress");
    assert_eq!(
        scheduling.progress_fingerprint.as_deref(),
        Some(fingerprint.as_str())
    );
    assert_eq!(scheduling.unchanged_progress_count, 2);
    assert_eq!(
        reconcile(&driver, &input, "runnable-revision-1", 31),
        PlanReconciliationOutcome::Paused {
            reason: PlanPauseReason::NoProgress
        }
    );
}

#[test]
fn a_missing_execution_state_never_manufactures_work_authority_or_a_retry_cycle() {
    let pool = pool();
    let driver = PlanReconciliationDriver::new(pool);
    assert!(
        driver
            .begin_with_wake(
                "missing-session",
                "new-cycle",
                &SessionWakeSignal::Callback,
                1
            )
            .is_err()
    );
    assert!(
        !driver
            .waiting_retry_for_active_cycle("missing-session", "retry")
            .expect("no active driver")
    );
    assert!(
        driver
            .projection("missing-session")
            .expect("no phase")
            .is_none()
    );
}

#[test]
fn ordinary_finish_persists_completed_and_suppresses_late_unrelated_callbacks() {
    let pool = pool();
    let before = authorized(&pool);
    let driver = PlanReconciliationDriver::new(Arc::clone(&pool));
    let mut input = unfinished();
    reconcile(&driver, &input, "runnable-revision-1", 10);
    let progress = state(&pool).scheduling.expect("progress");
    input.plan_terminal = true;
    input.executable_work = false;
    assert_eq!(
        reconcile(&driver, &input, "completed-work", 11),
        PlanReconciliationOutcome::Decision(PlanReconciliationDecision::Finish)
    );
    let completed = state(&pool);
    assert_eq!(completed.phase, SessionExecutionPhase::Completed);
    assert_authority(&before, &completed);
    let scheduling = completed.scheduling.as_ref().expect("scheduling");
    assert_eq!(scheduling.readiness, SessionReadiness::Completed);
    assert_eq!(
        scheduling.progress_fingerprint,
        progress.progress_fingerprint
    );
    assert_eq!(
        scheduling.unchanged_progress_count,
        progress.unchanged_progress_count
    );
    let projection = phase(&driver);
    assert_eq!(projection.phase, DriverPhase::Terminal);
    let restarted = PlanReconciliationDriver::new(Arc::clone(&pool));
    for signal in [
        SessionWakeSignal::Automatic,
        SessionWakeSignal::Recovery,
        SessionWakeSignal::Callback,
        SessionWakeSignal::ExternalCompletion {
            source_id: "late-background".to_owned(),
            origin_cycle_id: "origin-cycle".to_owned(),
        },
    ] {
        assert_eq!(
            restarted
                .begin_with_wake("ses", "new-cycle", &signal, 12)
                .expect("completed gate"),
            None
        );
        assert_eq!(state(&pool), completed);
        assert_eq!(phase(&restarted), projection);
    }
}

#[test]
fn host_background_wait_uses_stable_execution_identity_and_keeps_no_progress_trace() {
    let pool = pool();
    let before = authorized(&pool);
    let driver = PlanReconciliationDriver::new(Arc::clone(&pool));
    let mut input = unfinished();
    reconcile(&driver, &input, "same-runnable-state", 10);
    reconcile(&driver, &input, "same-runnable-state", 11);
    let prior = state(&pool).scheduling.expect("progress");
    input.background_wait = Some(external("bg_123", "origin-cycle"));
    assert_eq!(
        reconcile(&driver, &input, "an-observer-is-not-new-work-progress", 12),
        PlanReconciliationOutcome::Waiting {
            wait: external("bg_123", "origin-cycle")
        }
    );
    let waiting = state(&pool);
    assert_eq!(waiting.phase, SessionExecutionPhase::Waiting);
    assert_authority(&before, &waiting);
    let scheduling = waiting.scheduling.as_ref().expect("scheduling");
    assert_eq!(scheduling.progress_fingerprint, prior.progress_fingerprint);
    assert_eq!(scheduling.unchanged_progress_count, 2);
    assert_eq!(phase(&driver).unchanged_progress_count, 2);
    for (source_id, cycle_id) in [
        ("background:bg_123:9000", "origin-cycle"), // receipt identity, not execution identity
        ("bg_123", "different-cycle"),
    ] {
        assert_eq!(
            driver
                .begin_with_wake(
                    "ses",
                    "callback-cycle",
                    &SessionWakeSignal::ExternalCompletion {
                        source_id: source_id.to_owned(),
                        origin_cycle_id: cycle_id.to_owned(),
                    },
                    13
                )
                .expect("unrelated receipt or cycle"),
            None
        );
    }
    assert_eq!(state(&pool), waiting);
    driver
        .begin_with_wake(
            "ses",
            "callback-cycle",
            &SessionWakeSignal::ExternalCompletion {
                source_id: "bg_123".to_owned(),
                origin_cycle_id: "origin-cycle".to_owned(),
            },
            14,
        )
        .expect("matching execution completed")
        .expect("admitted");
    assert_eq!(
        state(&pool)
            .scheduling
            .expect("progress")
            .unchanged_progress_count,
        2
    );
    input.background_wait = None;
    assert_eq!(
        reconcile(&driver, &input, "same-runnable-state", 15),
        PlanReconciliationOutcome::Paused {
            reason: PlanPauseReason::NoProgress
        },
        "a completion without changed runnable state does not fabricate progress"
    );
}

#[test]
fn invalid_background_wait_does_not_mutate_scheduling_or_append_an_event() {
    for invalid in [
        human("question-id"),
        external("", "origin-cycle"),
        external("bg_123", ""),
    ] {
        let pool = pool();
        let before = authorized(&pool);
        let driver = PlanReconciliationDriver::new(Arc::clone(&pool));
        let mut input = unfinished();
        input.background_wait = Some(invalid);
        assert!(
            driver
                .reconcile_with_progress("ses", "origin-cycle", &input, true, "work", 10)
                .is_err()
        );
        assert_eq!(state(&pool), before);
        assert!(driver.projection("ses").expect("no event").is_none());
    }
}

#[test]
fn completed_state_and_terminal_event_commit_together() {
    let pool = pool();
    authorized(&pool);
    let driver = PlanReconciliationDriver::new(Arc::clone(&pool));
    let mut input = unfinished();
    reconcile(&driver, &input, "runnable-revision-1", 10);
    let before = state(&pool);
    let projection = phase(&driver);
    input.executable_work = false;
    input.plan_terminal = true;
    pool.get()
        .expect("connection")
        .execute_batch(
            "CREATE TRIGGER reject_driver_terminal BEFORE INSERT ON event
         WHEN NEW.type='session.driver.phase.1'
         BEGIN SELECT RAISE(ABORT,'terminal event refused'); END;",
        )
        .expect("terminal event failure");
    assert!(
        driver
            .reconcile_with_progress("ses", "origin-cycle", &input, true, "finished", 11)
            .is_err()
    );
    assert_eq!(state(&pool), before);
    assert_eq!(phase(&driver), projection);
}

#[test]
fn authoritative_cycle_survives_plan_handoff_and_new_work_control_admission() {
    for signal in [
        SessionWakeSignal::Callback,
        SessionWakeSignal::Recovery,
        SessionWakeSignal::Automatic,
        SessionWakeSignal::ExplicitResume,
    ] {
        let pool = pool();
        authorized(&pool);
        plan_mode(&pool);
        let driver = PlanReconciliationDriver::new(Arc::clone(&pool));
        driver.begin("ses", "old-plan-cycle").expect("Plan begin");
        let mut handoff = unfinished();
        handoff.planning_handoff = true;
        assert_eq!(
            reconcile(&driver, &handoff, "plan-ready", 10),
            PlanReconciliationOutcome::Decision(PlanReconciliationDecision::Finish)
        );
        assert_eq!(phase(&driver).cycle_id, "old-plan-cycle");
        assert_eq!(phase(&driver).phase, DriverPhase::Terminal);

        // The StartWork service has committed a new, explicitly authorized
        // continuation. The old terminal Plan event is still the last projection.
        let mut control = state(&pool);
        control.mode = CollaborationMode::Work;
        control.phase = SessionExecutionPhase::Authorized;
        control.cycle_id = Some("authorized-work-cycle".to_owned());
        let continuation = control.continuation.as_mut().expect("continuation");
        continuation.mode = CollaborationMode::Work;
        continuation.cycle_id = "authorized-work-cycle".to_owned();
        control.scheduling = Some(SessionScheduling::default());
        let control = pool
            .transaction(|tx| session_execution::update_in(tx, control.revision, control))
            .expect("commit explicit Work control");
        let proposed = if signal == SessionWakeSignal::ExplicitResume {
            "authorized-work-cycle"
        } else {
            "old-plan-cycle"
        };
        assert_eq!(
            driver
                .begin_with_wake("ses", proposed, &signal, 20)
                .expect("admit"),
            Some("authorized-work-cycle".to_owned()),
            "{signal:?} must not adopt the stale terminal Plan projection"
        );
        let admitted = state(&pool);
        assert_authority(&control, &admitted);
        assert_eq!(admitted.scheduling, control.scheduling);
        assert_eq!(phase(&driver).cycle_id, "authorized-work-cycle");
        assert_eq!(
            driver
                .reconcile_with_progress(
                    "ses",
                    "old-plan-cycle",
                    &unfinished(),
                    true,
                    "runnable-work",
                    21,
                )
                .expect("reconcile authorized Work"),
            PlanReconciliationOutcome::Decision(PlanReconciliationDecision::ContinueOrdinary {
                attempt: 1
            })
        );
        assert_eq!(
            state(&pool).cycle_id.as_deref(),
            Some("authorized-work-cycle")
        );
        assert_eq!(
            state(&pool).continuation.expect("continuation").cycle_id,
            "authorized-work-cycle"
        );
        assert_eq!(phase(&driver).cycle_id, "authorized-work-cycle");
    }
}

#[test]
fn authoritative_cycle_is_bound_with_existing_continuation_before_begin_event() {
    let pool = pool();
    authorized(&pool);
    let driver = PlanReconciliationDriver::new(Arc::clone(&pool));
    reconcile(&driver, &unfinished(), "same-runnable-state", 10);
    let before = state(&pool);
    pool.get()
        .expect("connection")
        .execute_batch(
            "CREATE TRIGGER require_bound_driver_cycle BEFORE INSERT ON event
         WHEN NEW.type='session.driver.phase.1'
           AND json_extract(NEW.data,'$.phase')='executing'
         BEGIN
           SELECT CASE WHEN
             (SELECT cycle_id FROM session_execution_state WHERE session_id=NEW.aggregate_id)
               IS NOT json_extract(NEW.data,'$.cycleId')
             OR
             (SELECT json_extract(continuation,'$.cycleId')
                FROM session_execution_state WHERE session_id=NEW.aggregate_id)
               IS NOT json_extract(NEW.data,'$.cycleId')
             THEN RAISE(ABORT,'driver event preceded durable cycle binding') END;
         END;",
        )
        .expect("require atomic cycle binding");
    assert_eq!(
        driver
            .begin_with_wake("ses", "new-user-cycle", &SessionWakeSignal::UserQuery, 20)
            .expect("begin user cycle"),
        Some("new-user-cycle".to_owned())
    );
    let bound = state(&pool);
    let mut expected = before.clone();
    expected.cycle_id = Some("new-user-cycle".to_owned());
    expected
        .continuation
        .as_mut()
        .expect("continuation")
        .cycle_id = "new-user-cycle".to_owned();
    assert_authority(&expected, &bound);
    assert_eq!(
        bound.scheduling, before.scheduling,
        "binding a cycle is not progress"
    );
    assert_eq!(phase(&driver).cycle_id, "new-user-cycle");
    assert_eq!(
        reconcile(&driver, &unfinished(), "same-runnable-state", 21),
        PlanReconciliationOutcome::Decision(PlanReconciliationDecision::ContinueOrdinary {
            attempt: 2
        })
    );
    assert_eq!(state(&pool).cycle_id.as_deref(), Some("new-user-cycle"));
    assert_eq!(phase(&driver).cycle_id, "new-user-cycle");
}

#[test]
fn authoritative_cycle_is_preserved_for_a_gated_user_query_with_a_stale_projection() {
    let pool = pool();
    authorized(&pool);
    let driver = PlanReconciliationDriver::new(Arc::clone(&pool));
    no_progress_pause(&driver);
    let projection = phase(&driver);
    let mut current = state(&pool);
    current.cycle_id = Some("current-paused-cycle".to_owned());
    current
        .continuation
        .as_mut()
        .expect("continuation")
        .cycle_id = "current-paused-cycle".to_owned();
    let current = pool
        .transaction(|tx| session_execution::update_in(tx, current.revision, current))
        .expect("authoritative current paused cycle");
    assert_eq!(
        driver
            .begin_with_wake("ses", "query-proposal", &SessionWakeSignal::UserQuery, 20)
            .expect("admit status query"),
        Some("current-paused-cycle".to_owned())
    );
    assert_eq!(state(&pool), current);
    assert_eq!(
        phase(&driver),
        projection,
        "a gated query does not begin automatic work"
    );
}

#[test]
fn user_cycle_binding_rolls_back_with_a_failed_begin_event() {
    let pool = pool();
    authorized(&pool);
    let driver = PlanReconciliationDriver::new(Arc::clone(&pool));
    reconcile(&driver, &unfinished(), "runnable-state", 10);
    let before = state(&pool);
    let projection = phase(&driver);
    pool.get()
        .expect("connection")
        .execute_batch(
            "CREATE TRIGGER reject_cycle_begin BEFORE INSERT ON event
         WHEN NEW.type='session.driver.phase.1'
         BEGIN SELECT RAISE(ABORT,'begin event refused'); END;",
        )
        .expect("begin failure");
    assert!(
        driver
            .begin_with_wake("ses", "new-user-cycle", &SessionWakeSignal::UserQuery, 20)
            .is_err()
    );
    assert_eq!(state(&pool), before);
    assert_eq!(phase(&driver), projection);
}

#[test]
fn binding_repairs_only_the_existing_continuation_cycle_and_never_creates_authority() {
    let pool = pool();
    let mut current = authorized(&pool);
    current.cycle_id = Some("authoritative-cycle".to_owned());
    let current = pool
        .transaction(|tx| session_execution::update_in(tx, current.revision, current))
        .expect("state cycle advances before a stale continuation is observed");
    let driver = PlanReconciliationDriver::new(Arc::clone(&pool));
    assert_eq!(
        driver
            .begin_with_wake("ses", "stale-proposal", &SessionWakeSignal::Recovery, 10)
            .expect("recovery"),
        Some("authoritative-cycle".to_owned())
    );
    let mut expected = current;
    expected
        .continuation
        .as_mut()
        .expect("continuation")
        .cycle_id = "authoritative-cycle".to_owned();
    let bound = state(&pool);
    assert_authority(&expected, &bound);
    assert_eq!(bound.scheduling, expected.scheduling);

    let fresh = super::tests::pool();
    let driver = PlanReconciliationDriver::new(Arc::clone(&fresh));
    let before = state(&fresh);
    driver
        .begin("ses", "fresh-user-cycle")
        .expect("first user cycle");
    let bound = state(&fresh);
    assert_eq!(bound.cycle_id.as_deref(), Some("fresh-user-cycle"));
    assert!(
        bound.continuation.is_none(),
        "the driver does not mint continuation authority"
    );
    assert_eq!(bound.scheduling, before.scheduling);
    assert_eq!(bound.work_identity, before.work_identity);
    assert_eq!(bound.authorized_plan_id, before.authorized_plan_id);
}

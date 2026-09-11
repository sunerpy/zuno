use serde_json::json;
use zuno_types::execution::{
    SessionExecutionPhase, SessionExecutionState, SessionPauseReason, SessionReadiness,
    SessionScheduling, SessionWaitReference, SessionWakeSignal, WakeAdmission,
};

fn answer(request_id: &str) -> SessionWakeSignal {
    SessionWakeSignal::UserAnswer {
        request_id: request_id.to_owned(),
    }
}

fn completion(source_id: &str, origin_cycle_id: &str) -> SessionWakeSignal {
    SessionWakeSignal::ExternalCompletion {
        source_id: source_id.to_owned(),
        origin_cycle_id: origin_cycle_id.to_owned(),
    }
}

fn readiness_states() -> Vec<SessionReadiness> {
    vec![
        SessionReadiness::Ready,
        SessionReadiness::WaitingHuman {
            request_id: "request-1".to_owned(),
        },
        SessionReadiness::WaitingExternal {
            source_id: "background-1".to_owned(),
            origin_cycle_id: "cycle-1".to_owned(),
        },
        SessionReadiness::Paused {
            reason: SessionPauseReason::NoProgress,
        },
        SessionReadiness::Completed,
    ]
}

#[test]
fn pure_admission_matrix_separates_user_queries_from_gate_resumption() {
    use WakeAdmission::{Admit, Reject, Resume};
    // The columns are ready, waiting-human, waiting-external, paused, completed.
    let cases = [
        (
            SessionWakeSignal::UserQuery,
            [Admit, Admit, Admit, Admit, Resume],
        ),
        (
            SessionWakeSignal::ExplicitResume,
            [Admit, Reject, Reject, Resume, Resume],
        ),
        (
            SessionWakeSignal::Automatic,
            [Admit, Reject, Reject, Reject, Reject],
        ),
        (
            SessionWakeSignal::Recovery,
            [Admit, Reject, Reject, Reject, Reject],
        ),
        (
            SessionWakeSignal::Callback,
            [Admit, Reject, Reject, Reject, Reject],
        ),
        (answer("request-1"), [Admit, Resume, Reject, Reject, Resume]),
        (
            answer("other-request"),
            [Admit, Reject, Reject, Reject, Resume],
        ),
        (answer(""), [Reject; 5]),
        (answer(" \t"), [Reject; 5]),
        (
            completion("background-1", "cycle-1"),
            [Admit, Reject, Resume, Reject, Reject],
        ),
        (
            completion("other-background", "cycle-1"),
            [Admit, Reject, Reject, Reject, Reject],
        ),
        (
            completion("background-1", "old-cycle"),
            [Admit, Reject, Reject, Reject, Reject],
        ),
        (completion("", "cycle-1"), [Reject; 5]),
        (completion("background-1", ""), [Reject; 5]),
    ];
    for (signal, expected) in cases {
        for (readiness, expected) in readiness_states().into_iter().zip(expected) {
            assert_eq!(
                readiness.wake_admission(&signal),
                expected,
                "{readiness:?} + {signal:?}"
            );
        }
    }
}

#[test]
fn every_pause_reason_requires_explicit_resume_and_keeps_query_admission() {
    for reason in [
        SessionPauseReason::NoProgress,
        SessionPauseReason::NoExecutableWork,
        SessionPauseReason::User,
        SessionPauseReason::Authentication,
        SessionPauseReason::TurnBudget,
        SessionPauseReason::Blocked,
    ] {
        let readiness = SessionReadiness::Paused { reason };
        for signal in [
            SessionWakeSignal::Automatic,
            SessionWakeSignal::Recovery,
            SessionWakeSignal::Callback,
            answer("request-1"),
            completion("background-1", "cycle-1"),
        ] {
            assert_eq!(readiness.wake_admission(&signal), WakeAdmission::Reject);
        }
        assert_eq!(
            readiness.wake_admission(&SessionWakeSignal::UserQuery),
            WakeAdmission::Admit
        );
        assert_eq!(
            readiness.wake_admission(&SessionWakeSignal::ExplicitResume),
            WakeAdmission::Resume
        );
    }
}

#[test]
fn scheduling_wire_preserves_session_progress_and_typed_wait_identity() {
    for readiness in readiness_states() {
        let scheduling = SessionScheduling {
            readiness,
            progress_fingerprint: Some("sha256:unchanged".to_owned()),
            unchanged_progress_count: 3,
        };
        let wire = serde_json::to_value(&scheduling).expect("encode");
        assert_eq!(wire["progressFingerprint"], "sha256:unchanged");
        assert_eq!(wire["unchangedProgressCount"], 3);
        assert_eq!(
            serde_json::from_value::<SessionScheduling>(wire).expect("decode"),
            scheduling
        );
    }
    let external = SessionReadiness::from(SessionWaitReference::External {
        source_id: "background-1".to_owned(),
        origin_cycle_id: "cycle-1".to_owned(),
    });
    assert_eq!(
        serde_json::to_value(external).expect("encode external wait"),
        json!({
            "kind": "waiting_external",
            "sourceId": "background-1",
            "originCycleId": "cycle-1"
        })
    );
    assert_eq!(
        SessionReadiness::from(SessionWaitReference::Human {
            request_id: "request-1".to_owned()
        }),
        SessionReadiness::WaitingHuman {
            request_id: "request-1".to_owned()
        }
    );
}

#[test]
fn legacy_phase_fallback_does_not_invent_waits_or_automatic_authority() {
    for phase in [
        SessionExecutionPhase::Idle,
        SessionExecutionPhase::Planning,
        SessionExecutionPhase::Authorized,
        SessionExecutionPhase::Running,
        SessionExecutionPhase::Waiting,
        SessionExecutionPhase::Paused,
        SessionExecutionPhase::Blocked,
        SessionExecutionPhase::Completed,
    ] {
        let state: SessionExecutionState = serde_json::from_value(json!({
            "sessionId": "ordinary-session",
            "revision": 1,
            "mode": "work",
            "phase": phase,
            "timeCreated": 1,
            "timeUpdated": 1
        }))
        .expect("legacy state without scheduling");
        assert!(state.scheduling.is_none());
        assert_eq!(
            state.wake_admission(&SessionWakeSignal::UserQuery),
            if phase == SessionExecutionPhase::Completed {
                WakeAdmission::Resume
            } else {
                WakeAdmission::Admit
            }
        );
        let gated = matches!(
            phase,
            SessionExecutionPhase::Waiting
                | SessionExecutionPhase::Paused
                | SessionExecutionPhase::Blocked
                | SessionExecutionPhase::Completed
        );
        for signal in [
            SessionWakeSignal::Automatic,
            SessionWakeSignal::Recovery,
            SessionWakeSignal::Callback,
            completion("background-1", "cycle-1"),
        ] {
            assert_eq!(
                state.wake_admission(&signal),
                if gated {
                    WakeAdmission::Reject
                } else {
                    WakeAdmission::Admit
                },
                "{phase:?} + {signal:?}"
            );
        }
        assert_eq!(
            state.wake_admission(&answer("unknown-request")),
            if phase == SessionExecutionPhase::Completed {
                WakeAdmission::Resume
            } else if gated {
                WakeAdmission::Reject
            } else {
                WakeAdmission::Admit
            }
        );
        if phase == SessionExecutionPhase::Waiting {
            assert_eq!(
                state.wake_admission(&SessionWakeSignal::ExplicitResume),
                WakeAdmission::Reject
            );
        }
    }
}

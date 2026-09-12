use super::*;
use zuno_db::event_log::{NewSessionEvent, SessionEventLog};
use zuno_db::message::{MessageRecord, MessageStore, PartRecord};

fn message(fixture: &Fixture, id: &str, role: &str, parent: Option<&str>) {
    let mut data = json!({
        "id": id, "sessionID": "ses_child", "role": role,
        "time": {"created": 100}, "agent": "worker",
    });
    if let Some(parent) = parent {
        data["parentID"] = json!(parent);
    }
    MessageStore::new(&fixture.connection())
        .put_message(&MessageRecord::from_json(data).unwrap())
        .unwrap();
}

fn part(fixture: &Fixture, id: &str, message_id: &str, kind: &str, status: &str) {
    MessageStore::new(&fixture.connection())
        .put_part_at(
            &PartRecord::from_json(
                json!({
                    "id": id, "sessionID": "ses_child", "messageID": message_id,
                    "type": kind, "tool": "read", "callID": format!("call_{id}"),
                    "text": "PRIVATE_TEXT_MUST_NOT_ENTER_PROGRESS",
                    "state": {
                        "status": status, "input": {}, "output": "PRIVATE_TOOL_BODY",
                    },
                }),
                100,
            )
            .unwrap(),
            100,
        )
        .unwrap();
}

fn provider_event(
    fixture: &Fixture,
    event_type: &str,
    request_id: &str,
    assistant: &str,
    status: &str,
    failure: Option<&str>,
) {
    let mut value = json!({
        "requestID": request_id, "assistantMessageID": assistant, "status": status,
        "message": "PRIVATE_RAW_ERROR", "reasoning": "PRIVATE_REASONING",
        "body": {"secret": "PRIVATE_REQUEST_BODY"},
    });
    if let Some(failure) = failure {
        value["errorKind"] = json!(failure);
    }
    SessionEventLog::new(Arc::clone(&fixture.host.database))
        .append(
            "ses_child",
            NewSessionEvent::new(event_type, value.as_object().unwrap().clone()).unwrap(),
        )
        .unwrap();
}

fn job(fixture: &Fixture, id: &str) -> AgentJob {
    let cursor = MessageStore::new(&fixture.connection())
        .latest_part_rowid_for_session("ses_child")
        .unwrap();
    fixture
        .host
        .job_store
        .create(
            NewAgentJob::new(
                id,
                "ses_owner",
                JobSubject::child_session("ses_child"),
                DbReportDelivery::Quiet,
                100,
            )
            .with_evidence_start_rowid(cursor),
        )
        .unwrap()
}

fn fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    fixture.session("ses_child", Some("ses_owner"));
    fixture
}

fn progress(fixture: &Fixture, job: &AgentJob) -> Value {
    serde_json::to_value(
        task_report_metadata_for_job(&fixture.host.database, job, "failed", "failed", Vec::new())
            .unwrap(),
    )
    .unwrap()["progress"]
        .clone()
}

fn successful_request(fixture: &Fixture, prefix: &str) {
    let user = format!("user_{prefix}");
    let assistant = format!("assistant_{prefix}");
    let request = format!("req_{prefix}");
    message(fixture, &user, "user", None);
    part(fixture, &format!("input_{prefix}"), &user, "text", "");
    message(fixture, &assistant, "assistant", Some(&user));
    provider_event(
        fixture,
        "session.provider.request",
        &request,
        &assistant,
        "started",
        None,
    );
    provider_event(
        fixture,
        "session.provider.request",
        &request,
        &assistant,
        "completed",
        None,
    );
    part(
        fixture,
        &format!("tool_{prefix}"),
        &assistant,
        "tool",
        "completed",
    );
}

#[test]
fn job_progress_counts_current_requests_including_failure_before_any_assistant_part() {
    let fixture = fixture();
    successful_request(&fixture, "historical");
    let job = job(&fixture, "job_current");
    successful_request(&fixture, "current");
    provider_event(
        &fixture,
        "session.provider.request",
        "req_current",
        "assistant_current",
        "completed",
        None,
    );
    part(
        &fixture,
        "failed_tool",
        "assistant_current",
        "tool",
        "error",
    );
    part(
        &fixture,
        "running_tool",
        "assistant_current",
        "tool",
        "running",
    );
    message(
        &fixture,
        "assistant_failed",
        "assistant",
        Some("user_current"),
    );
    provider_event(
        &fixture,
        "session.provider.request",
        "req_failed",
        "assistant_failed",
        "started",
        None,
    );
    for _ in 0..2 {
        provider_event(
            &fixture,
            "session.provider.attempt",
            "req_failed",
            "assistant_failed",
            "failed",
            Some("transient"),
        );
    }
    provider_event(
        &fixture,
        "session.provider.request",
        "req_failed",
        "assistant_failed",
        "failed",
        Some("provider_retry_deadline"),
    );
    fixture
        .host
        .job_store
        .settle(&job.id, JobSettlement::failed("failed", 1_595_100, None))
        .unwrap();

    let progress = progress(&fixture, &job);
    assert_eq!(
        progress,
        json!({
            "completedToolCalls": 1, "failedToolCalls": 1,
            "completedRequests": 1, "failedRequests": 1,
            "lastRequestId": "req_failed", "lastFailureKind": "provider_retry_deadline",
            "elapsedMs": 1_595_000,
        })
    );
    assert!(!progress.to_string().contains("PRIVATE"));
}

#[test]
fn old_job_progress_excludes_a_later_job_in_the_same_child() {
    let fixture = fixture();
    let first = job(&fixture, "job_first");
    successful_request(&fixture, "first");
    fixture
        .host
        .job_store
        .settle(&first.id, JobSettlement::failed("failed", 200, None))
        .unwrap();
    let _second = job(&fixture, "job_second");
    successful_request(&fixture, "second");
    part(
        &fixture,
        "later_failure",
        "assistant_second",
        "tool",
        "error",
    );

    let progress = progress(&fixture, &first);
    assert_eq!(progress["completedToolCalls"], 1);
    assert_eq!(progress["failedToolCalls"], 0);
    assert_eq!(progress["completedRequests"], 1);
    assert_eq!(progress["failedRequests"], 0);
    assert_eq!(progress["lastRequestId"], "req_first");
    assert_eq!(progress["elapsedMs"], 100);
}

#[test]
fn completed_request_does_not_retain_an_older_failure_kind() {
    let fixture = fixture();
    let job = job(&fixture, "job_replaced_terminal");
    successful_request(&fixture, "replaced_terminal");
    provider_event(
        &fixture,
        "session.provider.request",
        "req_replaced_terminal",
        "assistant_replaced_terminal",
        "failed",
        Some("provider_retry_deadline"),
    );
    provider_event(
        &fixture,
        "session.provider.request",
        "req_replaced_terminal",
        "assistant_replaced_terminal",
        "completed",
        None,
    );
    let progress = progress(&fixture, &job);
    assert_eq!(progress["completedRequests"], 1);
    assert_eq!(progress["failedRequests"], 0);
    assert_eq!(progress["lastRequestId"], "req_replaced_terminal");
    assert!(progress["lastFailureKind"].is_null(), "{progress}");
}

#[test]
fn missing_request_evidence_stays_unknown_and_does_not_parse_assistant_text() {
    let fixture = fixture();
    let job = job(&fixture, "job_legacy");
    message(&fixture, "legacy_user", "user", None);
    part(&fixture, "legacy_input", "legacy_user", "text", "");
    message(
        &fixture,
        "legacy_assistant",
        "assistant",
        Some("legacy_user"),
    );
    part(&fixture, "legacy_answer", "legacy_assistant", "text", "");
    let progress = progress(&fixture, &job);
    assert!(progress.is_object());
    for field in [
        "completedRequests",
        "failedRequests",
        "lastRequestId",
        "lastFailureKind",
    ] {
        assert!(progress[field].is_null(), "{field}: {progress}");
    }
    assert_eq!(progress["completedToolCalls"], 0);
    assert_eq!(progress["failedToolCalls"], 0);
}

#[test]
fn progress_without_a_verified_job_scope_is_unknown() {
    let fixture = fixture();
    successful_request(&fixture, "unattributed");
    let mut missing = job(&fixture, "job_scoped");
    missing.id = "job_missing".to_owned();
    let progress = progress(&fixture, &missing);
    assert_eq!(
        progress,
        json!({
            "completedToolCalls": null, "failedToolCalls": null,
            "completedRequests": null, "failedRequests": null,
            "lastRequestId": null, "lastFailureKind": null, "elapsedMs": null,
        })
    );
}

#[test]
fn progress_diagnostics_are_bounded_and_ignore_raw_error_fields() {
    let fixture = fixture();
    let job = job(&fixture, "job_bounded");
    successful_request(&fixture, "bounded");
    provider_event(
        &fixture,
        "session.provider.request",
        &"x".repeat(16_384),
        "assistant_bounded",
        "failed",
        Some(&"PRIVATE_RAW_ERROR".repeat(1_024)),
    );
    let progress = progress(&fixture, &job);
    assert!(progress.is_object());
    assert!(progress["lastRequestId"].is_null());
    assert!(progress["lastFailureKind"].is_null());
    assert!(progress.to_string().len() < 512, "{progress}");
    assert!(!progress.to_string().contains("PRIVATE"));
}

#[tokio::test]
async fn background_failure_report_retains_successful_progress_before_the_last_failure() {
    let fixture = fixture();
    let mut request = fixture.request("ses_owner");
    request.resume_session_id = Some("ses_child".to_owned());
    request.background = true;
    let turn = fixture
        .host
        .dispatch(request, no_interrupt())
        .await
        .unwrap();
    fixture.runner.wait_for_starts(1).await;
    successful_request(&fixture, "background");
    message(
        &fixture,
        "assistant_background_failed",
        "assistant",
        Some("user_background"),
    );
    provider_event(
        &fixture,
        "session.provider.request",
        "req_background_failed",
        "assistant_background_failed",
        "failed",
        Some("provider_retry_deadline"),
    );
    fixture.runner.complete_with(Err("provider failed"));
    fixture.jobs.wait_all().await;

    let job = fixture
        .host
        .job_store
        .get(turn.job_id.as_deref().unwrap())
        .unwrap();
    let metadata = job.result.as_ref().unwrap();
    assert_eq!(metadata["progress"]["completedRequests"], 1);
    assert_eq!(metadata["progress"]["failedRequests"], 1);
    let reports = fixture.wake.reports.lock().unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].prompt["metadata"], *metadata);
    let text = reports[0].prompt["text"].as_str().unwrap();
    assert!(text.contains("1 completed request"), "{text}");
    assert!(text.contains("1 failed request"), "{text}");
    assert!(text.contains("1 completed tool call"), "{text}");
    assert!(!text.contains("PRIVATE"), "{text}");
}

//! The render-time projection every settled-report delivery shares.

use serde_json::json;
use zuno_db::inbox::{InputDelivery, SessionInput, SubmissionState};
use zuno_engine::planning::PlanningInputSource;
use zuno_engine::report::ReportBatch;

fn promoted_report(id: &str, prompt: serde_json::Value, completed: i64, seq: i64) -> SessionInput {
    SessionInput {
        id: id.to_owned(),
        session_id: "ses_parent".to_owned(),
        prompt,
        delivery: InputDelivery::Queue,
        state: SubmissionState::Promoted,
        revision: 2,
        admitted_sequence: seq,
        promoted_sequence: Some(seq + 100),
        error: None,
        time_created: completed,
        time_updated: completed,
    }
}

fn job_report(id: &str, job_id: &str, status: &str, text: &str, completed: i64) -> SessionInput {
    promoted_report(
        id,
        json!({
            "kind": "subagentReport",
            "jobID": job_id,
            "childSessionID": "ses_child",
            "status": status,
            "text": text
        }),
        completed,
        completed,
    )
}

#[test]
fn a_single_report_batch_reaches_the_model_exactly_as_its_writer_wrote_it() {
    let batch = ReportBatch::project(&[job_report(
        "input_1",
        "job_1",
        "completed",
        "child finished",
        20,
    )]);

    let promoted = batch.reports();
    assert_eq!(promoted.len(), 1);
    assert_eq!(promoted[0].input_id, "input_1");
    assert_eq!(
        promoted[0].text, "child finished",
        "the common one-report delivery must carry no batch annotation"
    );
    assert!(promoted[0].newest);
    assert_eq!(promoted[0].source, PlanningInputSource::ChildReport);
    assert!(batch.undecodable().is_empty());
}

#[test]
fn reports_for_different_work_are_all_current_and_none_is_annotated() {
    let batch = ReportBatch::project(&[
        job_report("input_1", "job_1", "completed", "first done", 20),
        job_report("input_2", "job_2", "completed", "second done", 21),
    ]);

    let promoted = batch.reports();
    assert_eq!(
        promoted
            .iter()
            .map(|report| report.text.as_str())
            .collect::<Vec<_>>(),
        ["first done", "second done"],
        "a fan-out of distinct work supersedes nothing"
    );
    assert_eq!(
        promoted
            .iter()
            .map(|report| report.newest)
            .collect::<Vec<_>>(),
        [false, true],
        "the newest report seeds plan reconciliation"
    );
}

#[test]
fn repeated_reports_for_one_job_present_only_the_newest_as_current() {
    let batch = ReportBatch::project(&[
        job_report("input_1", "job_1", "uncertain", "child was interrupted", 20),
        job_report("input_2", "job_1", "completed", "child finished", 30),
        job_report("input_3", "job_2", "completed", "other work done", 25),
    ]);

    let promoted = batch.reports();
    assert!(
        promoted[0].text.starts_with(
            "[superseded report] Work `job_1` reported again later in this same delivery;"
        ),
        "an intermediate state must not read as the current one: {}",
        promoted[0].text
    );
    assert!(
        promoted[0].text.ends_with("\n\nchild was interrupted"),
        "the writer's exact text stays intact below the annotation: {}",
        promoted[0].text
    );
    assert_eq!(
        promoted[1].text,
        "[current report] Newest of 2 reports for work `job_1` in this delivery.\n\nchild finished"
    );
    assert_eq!(
        promoted[2].text, "other work done",
        "an unrelated job keeps its own untouched text"
    );
    assert_eq!(
        promoted
            .iter()
            .map(|report| report.newest)
            .collect::<Vec<_>>(),
        [false, true, false],
        "the newest completion in the whole batch seeds plan reconciliation"
    );
    assert_eq!(
        promoted
            .iter()
            .map(|report| report.input_id.as_str())
            .collect::<Vec<_>>(),
        ["input_1", "input_2", "input_3"],
        "every report keeps its own durable message id; nothing is merged away"
    );
}

#[test]
fn a_later_report_admitted_before_an_earlier_one_is_still_the_current_state() {
    let batch = ReportBatch::project(&[
        job_report("input_late_row", "job_1", "completed", "child finished", 40),
        job_report(
            "input_early_row",
            "job_1",
            "running",
            "child still going",
            10,
        ),
    ]);

    let promoted = batch.reports();
    assert!(
        promoted[0].text.starts_with("[current report]"),
        "supersession follows when the work completed, not when the row was written: {}",
        promoted[0].text
    );
    assert!(promoted[1].text.starts_with("[superseded report]"));
    assert!(promoted[0].newest);
    assert!(!promoted[1].newest);
}

#[test]
fn a_background_execution_report_groups_on_its_execution_and_seeds_its_own_source() {
    let batch = ReportBatch::project(&[
        promoted_report(
            "msg_exec_1",
            json!({
                "kind": "backgroundExecutionReport",
                "executionID": "exec_1",
                "status": "completed",
                "text": "build finished"
            }),
            50,
            1,
        ),
        job_report("input_1", "job_1", "completed", "child finished", 20),
    ]);

    let promoted = batch.reports();
    assert_eq!(promoted[0].source, PlanningInputSource::BackgroundReport);
    assert_eq!(promoted[1].source, PlanningInputSource::ChildReport);
    assert_eq!(
        promoted[0].text, "build finished",
        "one report per execution needs no annotation"
    );
    assert!(promoted[0].newest);
}

#[test]
fn a_promoted_report_without_model_visible_text_is_reported_for_failed_settlement() {
    let batch = ReportBatch::project(&[
        promoted_report(
            "input_broken",
            json!({"kind": "subagentReport", "jobID": "job_1", "text": 7}),
            20,
            1,
        ),
        job_report("input_1", "job_2", "completed", "child finished", 21),
    ]);

    assert_eq!(
        batch.undecodable(),
        ["input_broken"],
        "an unreadable report must be settled instead of stalling the batch"
    );
    let promoted = batch.reports();
    assert_eq!(promoted.len(), 1);
    assert_eq!(promoted[0].input_id, "input_1");
    assert!(!batch.is_empty());
}

#[test]
fn a_batch_of_only_unreadable_reports_drives_nothing() {
    let batch = ReportBatch::project(&[promoted_report(
        "input_broken",
        json!({"kind": "workflowReport", "jobID": "job_1"}),
        20,
        1,
    )]);

    assert!(batch.is_empty());
    assert_eq!(batch.undecodable(), ["input_broken"]);
}

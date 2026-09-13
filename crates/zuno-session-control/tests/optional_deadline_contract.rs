use std::sync::Arc;
use zuno_session_control::QuestionService;
use zuno_tool::question::QuestionPort;
use zuno_types::question::{
    QuestionMode, QuestionOrigin, QuestionPrompt, QuestionPurpose, QuestionSpec,
};

#[tokio::test]
async fn optional_question_has_a_native_two_minute_deadline() {
    let pool = Arc::new(zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).unwrap());
    {
        let mut connection = pool.get().unwrap();
        zuno_db::migration::apply(&mut connection).unwrap();
        connection.execute_batch(
            "INSERT INTO project(id,worktree,time_created,time_updated,sandboxes)
             VALUES('deadline-project','/workspace',1,1,'[]');
             INSERT INTO session(id,project_id,slug,directory,title,version,time_created,time_updated)
             VALUES('deadline-session','deadline-project','deadline','/workspace','Deadline','test',1,1);"
        ).unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    zuno_goal::GoalStore::from_pool(pool.clone(), dir.path().to_path_buf()).unwrap();
    let receipt = QuestionService::new(pool)
        .open(QuestionSpec {
            origin: QuestionOrigin {
                session_id: "deadline-session".to_owned(),
                message_id: None,
                call_id: Some("deadline-call".to_owned()),
                turn_id: None,
                goal_id: None,
            },
            mode: QuestionMode::Deferred,
            purpose: QuestionPurpose::Clarification,
            questions: vec![
                QuestionPrompt::new("Optional preference?", "Preference", vec![]).into_request(),
            ],
            expected_goal_revision: None,
            plan: None,
        })
        .await
        .unwrap();
    let json = serde_json::to_value(&receipt.question).unwrap();
    assert_eq!(
        json["autoDefer"]["deadlineAt"].as_i64(),
        Some(receipt.question.time_created + 120_000),
        "ordinary questions need an actual persisted deadline, not an indefinite form"
    );
}

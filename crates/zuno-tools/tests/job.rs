use serde_json::{Value, json};
use std::sync::Arc;
use zuno_db::job::{AgentJobStore, JobSettlement, NewAgentJob, ReportDelivery};
use zuno_db::{Pool, migration, session};
use zuno_paths::DbLocation;
use zuno_tool::{AllowAll, NeverInterrupted, Tool, ToolContext, erase};
use zuno_tools::job::JobTool;

const PARENT: &str = "ses_parent";
const OTHER_PARENT: &str = "ses_other";
const CHILD: &str = "ses_child";

fn initialized() -> Arc<Pool> {
    let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("open database"));
    {
        let mut connection = pool.get().expect("database connection");
        migration::apply(&mut connection).expect("apply schema");
        connection
            .execute(
                "INSERT INTO project \
                 (id, worktree, time_created, time_updated, sandboxes) \
                 VALUES ('project', '/workspace', 1, 1, '[]')",
                [],
            )
            .expect("create project");
    }
    pool.transaction(|transaction| {
        for (id, slug, parent) in [
            (PARENT, "parent", None),
            (OTHER_PARENT, "other", None),
            (CHILD, "child", Some(PARENT)),
        ] {
            let mut create = session::SessionCreate::new(
                id,
                slug,
                "project",
                "/workspace",
                "/workspace",
                slug,
                "zuno",
            )
            .at(1);
            if let Some(parent) = parent {
                create = create.with_parent(parent);
            }
            session::create(transaction, &create)?;
        }
        Ok(())
    })
    .expect("create sessions");
    pool
}

fn context(session_id: &str) -> ToolContext {
    ToolContext::new(
        session_id,
        "msg_parent",
        "call_job",
        "build",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
}

fn tool(pool: Arc<Pool>) -> Arc<dyn Tool> {
    erase(JobTool::new(pool))
}

fn output_json(output: &zuno_tool::ToolOutput) -> Value {
    serde_json::from_str(&output.output).expect("job output is JSON")
}

#[test]
fn the_tool_uses_the_native_job_wire_contract() {
    let erased = tool(initialized());

    assert_eq!(erased.id(), "job");
    let schema = erased.definition().parameters;
    assert!(schema["properties"].get("jobID").is_some(), "{schema}");
    assert!(schema["properties"].get("job_id").is_none(), "{schema}");
}

#[tokio::test]
async fn a_running_job_reports_its_durable_identity_and_delivery() {
    let pool = initialized();
    AgentJobStore::new(Arc::clone(&pool))
        .create(NewAgentJob::new(
            "job_running",
            PARENT,
            CHILD,
            ReportDelivery::NextStep,
            10,
        ))
        .expect("create job");

    let output = tool(pool)
        .execute(json!({"jobID": "job_running"}), context(PARENT))
        .await
        .expect("query running job");
    let body = output_json(&output);

    assert_eq!(output.title, "job_running: running");
    assert_eq!(body["jobID"], "job_running");
    assert_eq!(body["parentSessionID"], PARENT);
    assert_eq!(body["childSessionID"], CHILD);
    assert_eq!(body["status"], "running");
    assert_eq!(body["reportDelivery"], "nextStep");
    assert!(body["result"].is_null());
    assert!(body["error"].is_null());
}

#[tokio::test]
async fn a_completed_job_returns_the_persisted_result() {
    let pool = initialized();
    let store = AgentJobStore::new(Arc::clone(&pool));
    store
        .create(NewAgentJob::new(
            "job_completed",
            PARENT,
            CHILD,
            ReportDelivery::Quiet,
            10,
        ))
        .expect("create job");
    store
        .settle(
            "job_completed",
            JobSettlement::completed(json!({"text": "child result"}), 20, None),
        )
        .expect("settle job");

    let output = tool(pool)
        .execute(json!({"jobID": "job_completed"}), context(PARENT))
        .await
        .expect("query completed job");
    let body = output_json(&output);

    assert_eq!(output.title, "job_completed: completed");
    assert_eq!(body["result"], json!({"text": "child result"}));
    assert!(body["error"].is_null());
    assert_eq!(body["timeCompleted"], 20);
}

#[tokio::test]
async fn another_parents_job_is_indistinguishable_from_an_unknown_job() {
    let pool = initialized();
    AgentJobStore::new(Arc::clone(&pool))
        .create(NewAgentJob::new(
            "job_private",
            PARENT,
            CHILD,
            ReportDelivery::Quiet,
            10,
        ))
        .expect("create job");
    let erased = tool(pool);

    let private = erased
        .execute(json!({"jobID": "job_private"}), context(OTHER_PARENT))
        .await
        .expect_err("another parent cannot inspect this job");
    let absent = erased
        .execute(json!({"jobID": "job_absent"}), context(OTHER_PARENT))
        .await
        .expect_err("unknown job is rejected");

    assert_eq!(private.to_string(), absent.to_string());
    assert!(private.is_model_correctable());
    assert!(absent.is_model_correctable());
}

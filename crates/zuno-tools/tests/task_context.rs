use serde_json::{Value, json};
use std::sync::Arc;
use zuno_db::{Pool, migration};
use zuno_paths::DbLocation;
use zuno_tool::{AllowAll, NeverInterrupted, Tool, ToolContext};

const SESSION: &str = "task-context-session";

fn database() -> Arc<Pool> {
    let pool = Arc::new(Pool::open(&DbLocation::Memory).unwrap());
    let mut connection = pool.get().unwrap();
    migration::apply(&mut connection).unwrap();
    connection.execute_batch(
        "INSERT INTO project(id,worktree,time_created,time_updated,sandboxes)
         VALUES('task-context-project','/workspace',1,1,'[]');
         INSERT INTO session(id,project_id,slug,directory,title,version,time_created,time_updated)
         VALUES('task-context-session','task-context-project','task','/workspace','Task','test',1,1);
         INSERT INTO message(id,session_id,time_created,time_updated,data)
         VALUES('user-delivery','task-context-session',1,1,'{\"role\":\"user\"}'),
               ('user-no-announce','task-context-session',2,2,'{\"role\":\"user\"}'),
               ('assistant-not-authority','task-context-session',3,3,'{\"role\":\"assistant\"}');
         INSERT INTO part(id,message_id,session_id,time_created,time_updated,data)
         VALUES('source-delivery','user-delivery','task-context-session',1,1,
           '{\"type\":\"text\",\"text\":\"Implement and verify the release workflow until it works.\"}'),
          ('source-no-announce','user-no-announce','task-context-session',2,2,
           '{\"type\":\"text\",\"text\":\"Do not announce to Telegram.\"}');"
    ).unwrap();
    drop(connection);
    pool
}

fn tool(pool: Arc<Pool>) -> Arc<dyn Tool> {
    zuno_tools::work_state_tools(pool)
        .into_iter()
        .find(|tool| tool.id() == "task_context")
        .expect("native long-task context must be available with Work controls")
}

fn context(call: &str) -> ToolContext {
    ToolContext::new(
        SESSION,
        "assistant-context",
        call,
        "build",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
}

#[tokio::test]
async fn task_context_is_a_real_native_work_capability() {
    let tool = tool(database());
    let result = tool
        .execute(json!({"action":"get"}), context("read"))
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&result.output).unwrap();
    assert!(value["context"].is_null());
    assert_eq!(value["authority"], "user_sources_and_runtime_policy_only");
}

fn initial() -> Value {
    json!({
        "action":"update","objective":"Implement and verify the release pipeline",
        "task_kind":"delivery","authorized_actions":["repair CI","run validation","publish release"],
        "source_message_ids":["user-delivery"],
        "checks":[{"id":"release","kind":"delivery","description":"All promised outputs succeed","status":"pending"}]
    })
}

async fn call(tool: &Arc<dyn Tool>, args: Value, id: &str) -> Value {
    let output = tool.execute(args, context(id)).await.unwrap();
    serde_json::from_str(&output.output).unwrap()
}

#[tokio::test]
async fn refinements_preserve_authorized_work_and_survive_reopen() {
    let pool = database();
    let tool = tool(pool.clone());
    let first = call(&tool, initial(), "initial").await;
    assert_eq!(first["context"]["revision"], 1);
    let refinement = json!({"action":"update","expected_revision":1,
        "prohibitions":["Do not announce to Telegram"],"source_message_ids":["user-no-announce"]});
    let second = call(&tool, refinement.clone(), "refine").await;
    assert_eq!(
        second["context"]["authorizedActions"],
        first["context"]["authorizedActions"]
    );
    assert_eq!(
        second["context"]["objective"],
        first["context"]["objective"]
    );
    assert_eq!(
        second["context"]["prohibitions"],
        json!(["Do not announce to Telegram"])
    );
    assert_eq!(second["context"]["sources"].as_array().unwrap().len(), 2);
    let log_before: i64 = pool
        .get()
        .unwrap()
        .query_row("SELECT count(*) FROM event", [], |r| r.get(0))
        .unwrap();
    assert_eq!(call(&tool, refinement, "refine").await, second);
    assert_eq!(
        pool.get()
            .unwrap()
            .query_row("SELECT count(*) FROM event", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        log_before
    );
    let reopened = zuno_tools::work_state_tools(pool)
        .into_iter()
        .find(|t| t.id() == "task_context")
        .unwrap();
    assert_eq!(
        call(&reopened, json!({"action":"get"}), "reopen").await["context"],
        second["context"]
    );
}

#[tokio::test]
async fn safety_success_cannot_complete_unfinished_delivery_or_erase_its_check() {
    let tool = tool(database());
    call(&tool, initial(), "initial").await;
    let false_completion = json!({"action":"update","expected_revision":1,"status":"complete",
        "checks":[{"id":"fail-closed","kind":"safety","description":"Draft remains private on failure",
            "status":"passed","evidence":["fixture:negative-check"]}]});
    let error = tool
        .execute(false_completion, context("false-completion"))
        .await
        .unwrap_err();
    assert!(zuno_error::source::describe(&error).contains("delivery is incomplete"));
    let after = call(&tool, json!({"action":"get"}), "after-failure").await;
    assert_eq!(after["context"]["revision"], 1);
    let completed = call(
        &tool,
        json!({"action":"update","expected_revision":1,"status":"complete",
        "checks":[{"id":"release","kind":"delivery","description":"All promised outputs succeed",
            "status":"passed","evidence":["fixture:all-deliverables-green"]}]}),
        "complete",
    )
    .await;
    assert_eq!(completed["context"]["status"], "complete");
}

#[tokio::test]
async fn explicit_inspection_intent_and_source_validation_are_preserved() {
    let pool = database();
    let tool = tool(pool.clone());
    call(&tool, initial(), "initial").await;
    assert!(
        tool.execute(
            json!({"action":"update","expected_revision":1,"task_kind":"inspection",
        "source_message_ids":["assistant-not-authority"]}),
            context("invalid-source")
        )
        .await
        .is_err()
    );
    assert!(
        tool.execute(
            json!({"action":"update","expected_revision":99,"status":"complete"}),
            context("stale")
        )
        .await
        .is_err()
    );
    let inspection = call(
        &tool,
        json!({"action":"update","expected_revision":1,
        "task_kind":"inspection","authorized_actions":["inspect and report only"],
        "source_message_ids":["user-no-announce"]}),
        "inspect",
    )
    .await;
    assert_eq!(inspection["context"]["intent"], "inspection");
    assert_eq!(
        inspection["context"]["authorizedActions"],
        json!(["inspect and report only"])
    );
    // The tool records understanding, not an execution/Goal authority mutation.
    assert_eq!(
        pool.get()
            .unwrap()
            .query_row("SELECT count(*) FROM session_execution_state", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn technical_choices_cannot_be_marked_as_user_owned_blockers_implicitly() {
    let tool = tool(database());
    call(&tool, initial(), "initial").await;
    assert!(
        tool.execute(
            json!({"action":"update","expected_revision":1,"status":"waiting",
        "blocker":{"kind":"user_authority","reason":"Choose my implementation"}}),
            context("agent-owned")
        )
        .await
        .is_err()
    );
    assert!(
        tool.execute(
            json!({"action":"update","expected_revision":1,"status":"waiting"}),
            context("no-blocker")
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn task_context_sources_bind_text_and_never_change_execution_authority() {
    let pool = database();
    let tool = tool(pool.clone());
    call(&tool, initial(), "initial").await;
    assert_eq!(
        tool.effect(&initial()),
        zuno_tool::ToolEffect::ManagedContext
    );
    assert!(!tool.effect(&initial()).requires_manual_approval());
    let rendered = zuno_tools::task_context::runtime_context(&pool.get().unwrap(), SESSION)
        .unwrap()
        .unwrap();
    assert!(rendered.contains("\"sourcesCurrent\":true"));
    assert!(rendered.contains("\"newerUserMessageId\":\"user-no-announce\""));
    pool.get()
        .unwrap()
        .execute(
            "UPDATE part SET data=json_set(data,'$.text','Explicitly different instruction')
         WHERE id='source-delivery'",
            [],
        )
        .unwrap();
    let rendered = zuno_tools::task_context::runtime_context(&pool.get().unwrap(), SESSION)
        .unwrap()
        .unwrap();
    assert!(rendered.contains("\"sourcesCurrent\":false"));
    assert!(rendered.contains("\"historicalOnly\":true"));
}

#[tokio::test]
async fn child_cannot_rewrite_parent_decisions() {
    let pool = database();
    pool.get().unwrap().execute_batch(
        "INSERT INTO session(id,project_id,slug,directory,title,version,parent_id,time_created,time_updated)
         VALUES('child','task-context-project','child','/workspace','Child','test',
           'task-context-session',1,1);"
    ).unwrap();
    let tool = tool(pool);
    let child = ToolContext::new(
        "child",
        "message",
        "call",
        "general",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    );
    let error = tool.execute(initial(), child).await.unwrap_err();
    assert!(zuno_error::source::describe(&error).contains("parent"));
}

#[tokio::test]
async fn task_sources_exclude_compaction_and_reports_before_recent_limit() {
    let pool = database();
    let tool = tool(pool.clone());
    call(&tool, initial(), "initial").await;
    for i in 0..20 {
        pool.get().unwrap().execute(
            "INSERT INTO message(id,session_id,time_created,time_updated,data) VALUES(?1,?2,?3,?3,?4)",
            rusqlite::params![
                format!("non-user-{i}"), SESSION, 10 + i,
                json!({"role":"user","mode":"compaction"}).to_string(),
            ],
        ).unwrap();
    }
    pool.get().unwrap().execute(
        "INSERT INTO message(id,session_id,time_created,time_updated,data) VALUES('report',?1,100,100,?2)",
        rusqlite::params![SESSION, json!({"role":"user","taskReport":{}}).to_string()],
    ).unwrap();
    let listing = call(&tool, json!({"action":"get"}), "sources").await;
    let ids: Vec<_> = listing["userSources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|source| source["source"]["messageId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["user-no-announce", "user-delivery"]);
    for id in ["non-user-0", "report"] {
        assert!(
            tool.execute(
                json!({
                    "action":"update","expected_revision":1,"objective":"Changed",
                    "source_message_ids":[id],
                }),
                context(id)
            )
            .await
            .is_err()
        );
    }
    let rendered = zuno_tools::task_context::runtime_context(&pool.get().unwrap(), SESSION)
        .unwrap()
        .unwrap();
    assert!(rendered.contains("\"newerUserMessageId\":\"user-no-announce\""));
}

#[tokio::test]
async fn task_source_digest_includes_attachment_references() {
    let pool = database();
    pool.get()
        .unwrap()
        .execute(
            "INSERT INTO part(id,message_id,session_id,time_created,time_updated,data)
         VALUES('attachment','user-delivery',?1,1,1,?2)",
            rusqlite::params![
                SESSION,
                json!({"type":"file","mime":"text/plain","url":"file:///fixture/authorized.txt"})
                    .to_string()
            ],
        )
        .unwrap();
    let tool = tool(pool.clone());
    call(&tool, initial(), "initial").await;
    pool.get().unwrap().execute(
        "UPDATE part SET data=json_set(data,'$.url','file:///fixture/different.txt') WHERE id='attachment'", [],
    ).unwrap();
    let rendered = zuno_tools::task_context::runtime_context(&pool.get().unwrap(), SESSION)
        .unwrap()
        .unwrap();
    assert!(rendered.contains("\"sourcesCurrent\":false"));
}

#[tokio::test]
async fn task_delivery_definition_cannot_be_replaced_by_a_negative_check() {
    let tool = tool(database());
    call(&tool, initial(), "initial").await;
    let changed = json!({"action":"update","expected_revision":1,"status":"complete",
        "checks":[{"id":"release","kind":"delivery","description":"Failed draft stays private",
            "status":"passed","evidence":["fixture:negative"]}]});
    assert!(tool.execute(changed, context("rewrite")).await.is_err());
    let current = call(&tool, json!({"action":"get"}), "read").await;
    assert_eq!(current["context"]["revision"], 1);
}

#[tokio::test]
async fn check_definition_revision_requires_sources_and_invalidates_old_evidence() {
    let tool = tool(database());
    call(&tool, initial(), "initial").await;
    let revised_check = json!({"id":"release","kind":"delivery",
        "description":"Verify release assets without Telegram","status":"pending"});
    assert!(
        tool.execute(
            json!({"action":"update","expected_revision":1,
        "revise_checks":true,"checks":[revised_check.clone()]}),
            context("no-source")
        )
        .await
        .is_err()
    );
    let mut stale_evidence = revised_check.clone();
    stale_evidence["status"] = json!("passed");
    stale_evidence["evidence"] = json!(["fixture:old-evidence"]);
    assert!(
        tool.execute(
            json!({"action":"update","expected_revision":1,
        "revise_checks":true,"source_message_ids":["user-no-announce"],"checks":[stale_evidence]}),
            context("stale-evidence")
        )
        .await
        .is_err()
    );
    let revised = call(
        &tool,
        json!({"action":"update","expected_revision":1,
        "revise_checks":true,"source_message_ids":["user-no-announce"],"checks":[revised_check]}),
        "revise",
    )
    .await;
    assert_eq!(revised["context"]["checks"][0]["status"], "pending");
    assert_eq!(revised["context"]["checks"][0]["evidence"], json!([]));
}

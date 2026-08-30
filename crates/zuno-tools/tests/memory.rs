//! Model-facing proof for the durable `memory_propose` boundary.

use serde_json::{Value, json};
use std::sync::Arc;
use tempfile::TempDir;
use zuno_memory::{
    MemoryService, MemoryStore, Operation, PromotionPolicy, Scope, ScopeLimits, ScopePaths,
    SessionMemory,
};
use zuno_tool::{AllowAll, NeverInterrupted, Tool, ToolContext, erase};
use zuno_tools::{MEMORY_TOOL_ID, MemoryTool};

struct Fixture {
    _directory: TempDir,
    tool: Arc<dyn Tool>,
    service: Arc<MemoryService>,
    paths: ScopePaths,
}

impl Fixture {
    fn new() -> Self {
        let directory = TempDir::new().expect("temp dir");
        let paths = ScopePaths::at(
            directory.path().join("MEMORY.md"),
            directory.path().join("RULES.md"),
        );
        let pool =
            Arc::new(zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("open database"));
        let mut connection = pool.open_connection().expect("database connection");
        zuno_db::migration::apply(&mut connection).expect("initialize schema");
        connection
            .execute_batch(
                "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
                 VALUES ('project', '/tmp/project', 1, 1, '[]');
                 INSERT INTO session (
                     id, project_id, slug, directory, title, version, time_created, time_updated
                 ) VALUES (
                     'ses_integration', 'project', 'integration', '/tmp/project',
                     'Integration', '1', 1, 1
                 );",
            )
            .expect("seed tool session");
        drop(connection);
        let service = Arc::new(MemoryService::new(
            pool,
            paths.clone(),
            ScopeLimits::default(),
            PromotionPolicy::Review,
        ));
        Self {
            tool: erase(MemoryTool::new(Arc::clone(&service))),
            service,
            paths,
            _directory: directory,
        }
    }

    async fn call(&self, arguments: Value) -> zuno_tool::ToolOutput {
        self.tool
            .execute(arguments, context())
            .await
            .expect("valid proposal")
    }
}

fn context() -> ToolContext {
    ToolContext::new(
        "ses_integration",
        "msg_1",
        "call_1",
        "build",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
}

#[tokio::test]
async fn proposal_is_durable_but_does_not_change_the_resident_prompt_until_approved() {
    let fixture = Fixture::new();
    let convention = "run `cargo test -p <crate>` before the workspace suite";
    let output = fixture
        .call(json!({
            "target": "project",
            "action": "add",
            "content": convention,
            "reason": "repository validation convention",
            "confidence": 0.97
        }))
        .await;

    assert!(output.output.contains("pending"), "{}", output.output);
    let metadata = &output.metadata["memory_candidate"];
    let id = metadata["id"].as_str().expect("candidate id");
    assert_eq!(metadata["status"], "pending");
    assert!(!fixture.paths.for_scope(Scope::Project).exists());

    fixture.service.apply(id).expect("approve candidate");
    let prompt = SessionMemory::open(
        fixture.paths.for_scope(Scope::Global),
        fixture.paths.for_scope(Scope::Project),
    )
    .expect("memory stores")
    .inject_into("SYSTEM");
    assert!(prompt.contains(convention), "{prompt}");
}

#[tokio::test]
async fn cross_cutting_tool_fields_are_stripped_before_candidate_decoding() {
    let fixture = Fixture::new();
    let output = fixture
        .call(json!({
            "intent": "save a stable preference",
            "accept_large_output": true,
            "target": "global",
            "action": "add",
            "content": "explain changes before applying them",
            "reason": "explicit user preference",
            "confidence": 1.0
        }))
        .await;

    assert!(output.output.contains("pending"));
    assert_eq!(fixture.service.candidates().expect("candidates").len(), 1);
}

#[tokio::test]
async fn malformed_candidate_arguments_are_model_correctable_and_write_nothing() {
    let fixture = Fixture::new();
    let error = fixture
        .tool
        .execute(
            json!({
                "target": "project",
                "action": "add",
                "content": "missing reason and confidence"
            }),
            context(),
        )
        .await
        .expect_err("invalid proposal");

    assert_eq!(error.tool(), MEMORY_TOOL_ID);
    assert!(error.is_model_correctable());
    assert!(fixture.service.candidates().expect("candidates").is_empty());
    assert!(!fixture.paths.for_scope(Scope::Project).exists());
}

#[tokio::test]
async fn ambiguous_locator_is_rejected_before_a_candidate_is_inserted() {
    let fixture = Fixture::new();
    let mut store = MemoryStore::open(
        Scope::Project,
        fixture.paths.for_scope(Scope::Project).to_path_buf(),
    )
    .expect("store");
    store
        .apply_batch(&[
            Operation::add("build the api crate with `make build`"),
            Operation::add("build the web crate with `make build`"),
        ])
        .expect("seed entries");

    let error = fixture
        .tool
        .execute(
            json!({
                "target": "project",
                "action": "remove",
                "old_text": "`make build`",
                "reason": "retire a stale command",
                "confidence": 1.0
            }),
            context(),
        )
        .await
        .expect_err("ambiguous locator");

    let detail = zuno_error::source::describe(&error);
    assert!(detail.contains("matched 2 distinct entries"), "{detail}");
    assert!(fixture.service.candidates().expect("candidates").is_empty());
    assert_eq!(store.entries().len(), 2);
}

#[tokio::test]
async fn credential_literals_are_rejected_before_a_candidate_is_inserted() {
    let fixture = Fixture::new();
    let error = fixture
        .tool
        .execute(
            json!({
                "target": "global",
                "action": "add",
                "content": "sk-1234567890abcdefghijklmnop",
                "reason": "save the credential",
                "confidence": 1.0
            }),
            context(),
        )
        .await
        .expect_err("credential must be blocked");

    assert!(error.is_model_correctable());
    assert!(fixture.service.candidates().expect("candidates").is_empty());
    assert!(!fixture.paths.for_scope(Scope::Global).exists());
}

#[tokio::test]
async fn resident_storage_failures_are_not_reported_as_model_correctable_arguments() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.paths.for_scope(Scope::Project))
        .expect("replace resident file path with a directory");
    let error = fixture
        .tool
        .execute(
            json!({
                "target": "project",
                "action": "add",
                "content": "run cargo test",
                "reason": "verified repository gate",
                "confidence": 1.0
            }),
            context(),
        )
        .await
        .expect_err("resident storage failure");

    assert!(!error.is_model_correctable());
    assert!(fixture.service.candidates().expect("candidates").is_empty());
}

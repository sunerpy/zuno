//! The `memory` tool through the boundary a model actually reaches: erased to
//! `dyn Tool`, called with raw JSON, carrying the centrally injected `intent`.
//!
//! The unit tests call `run` with a decoded `MemoryParams`. That skips the two things
//! most likely to break silently after a schema change: whether real model JSON
//! deserializes into the dual shape at all, and whether the cross-cutting properties
//! Todo 38 injects are stripped before a `deny_unknown_fields` params struct sees
//! them. Both are exercised here.
//!
//! Nothing in this file resolves a scope through `oc_paths`; every store is a file in
//! a `TempDir`. A test that writes the developer's real `MEMORY.md` is a bug in the
//! test.

use oc_memory::{MemoryStore, Scope, SessionMemory};
use oc_tool::{AllowAll, NeverInterrupted, Tool, ToolContext, ToolOutput, erase};
use oc_tools::{MEMORY_TOOL_ID, MemoryTool, ScopePaths};
use serde_json::{Value, json};
use std::sync::Arc;
use tempfile::TempDir;

/// A worktree whose two memory files are both inside it.
struct Fixture {
    directory: TempDir,
    tool: Arc<dyn Tool>,
    paths: ScopePaths,
}

impl Fixture {
    fn new() -> Self {
        let directory = TempDir::new().expect("temp dir");
        let paths = ScopePaths::at(
            directory.path().join("MEMORY.md"),
            directory.path().join("RULES.md"),
        );
        Self {
            tool: erase(MemoryTool::with_paths(paths.clone())),
            paths,
            directory,
        }
    }

    async fn call(&self, arguments: Value) -> ToolOutput {
        self.tool
            .execute(arguments, context())
            .await
            .expect("a memory refusal is a response, never a failed turn")
    }

    fn store(&self, scope: Scope) -> MemoryStore {
        MemoryStore::open(scope, self.paths.for_scope(scope).to_path_buf()).expect("re-open")
    }

    /// A fresh session's frozen prompt blocks, as Todo 99 captures them.
    fn next_session_prompt(&self, base: &str) -> String {
        SessionMemory::open(
            self.paths.for_scope(Scope::Global),
            self.paths.for_scope(Scope::Project),
        )
        .expect("both stores load")
        .inject_into(base)
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

fn body(output: &ToolOutput) -> Value {
    serde_json::from_str(&output.output).expect("the response body is JSON")
}

#[tokio::test]
async fn a_saved_convention_appears_in_the_next_sessions_prompt_but_not_this_ones() {
    let fixture = Fixture::new();
    let convention = "run `cargo test -p <crate>`; the workspace suite is the merge gate only";
    let before = fixture.next_session_prompt("SYSTEM");

    let saved = fixture
        .call(json!({
            "intent": "record the test command",
            "target": "project",
            "operations": [{ "action": "add", "content": convention }],
        }))
        .await;

    assert_eq!(body(&saved)["success"], json!(true), "{}", saved.output);
    assert_eq!(
        before, "SYSTEM",
        "an empty store must add no prompt bytes at all"
    );

    let after = fixture.next_session_prompt("SYSTEM");
    assert!(after.contains(convention), "{after}");
    assert!(
        after.contains(Scope::Project.label()),
        "the block keeps its header so a consistency check can find it: {after}"
    );
    assert!(
        !after.contains(Scope::Global.label()),
        "the untouched global store must stay out of the prompt: {after}"
    );
}

#[tokio::test]
async fn a_locator_matching_two_entries_is_refused_naming_both() {
    let fixture = Fixture::new();
    for crate_name in ["api", "web"] {
        let response = fixture
            .call(json!({
                "intent": "record a build command",
                "target": "project",
                "action": "add",
                "content": format!("build the {crate_name} crate with `make build`"),
            }))
            .await;
        assert_eq!(body(&response)["success"], json!(true));
    }

    let refused = fixture
        .call(json!({
            "intent": "retire a stale build command",
            "target": "project",
            "action": "remove",
            "old_text": "`make build`",
        }))
        .await;
    let refused = body(&refused);
    let error = refused["error"].as_str().expect("an error");

    assert_eq!(refused["success"], json!(false));
    assert!(error.contains("matched 2 distinct entries"), "{error}");
    assert!(error.contains("api crate"), "{error}");
    assert!(error.contains("web crate"), "{error}");
    assert_eq!(
        refused["current_entries"]
            .as_array()
            .expect("the entries so the model can pick a unique locator")
            .len(),
        2
    );
    assert_eq!(
        fixture.store(Scope::Project).entries().len(),
        2,
        "an ambiguous locator must not have removed anything"
    );
}

#[tokio::test]
async fn the_injected_intent_never_reaches_the_params_struct() {
    let fixture = Fixture::new();

    // `MemoryParams` is `deny_unknown_fields`, so this call fails unless the central
    // augmentation's own properties are stripped before decoding.
    let saved = fixture
        .call(json!({
            "intent": "save a preference",
            "accept_large_output": true,
            "target": "global",
            "action": "add",
            "content": "explains the change before applying it",
        }))
        .await;

    assert_eq!(body(&saved)["success"], json!(true), "{}", saved.output);
    assert_eq!(fixture.store(Scope::Global).entries().len(), 1);
}

#[tokio::test]
async fn an_unusable_call_shape_is_a_model_correctable_argument_error() {
    let fixture = Fixture::new();

    let error = fixture
        .tool
        .execute(
            json!({ "intent": "save something", "target": "project" }),
            context(),
        )
        .await
        .expect_err("no change was requested, so there is nothing to report about memory");

    assert_eq!(error.tool(), MEMORY_TOOL_ID);
    assert!(error.is_model_correctable());
    assert!(
        fixture
            .directory
            .path()
            .join("RULES.md")
            .symlink_metadata()
            .is_err()
    );
}

#[tokio::test]
async fn a_batch_consolidates_and_adds_in_one_call() {
    let fixture = Fixture::new();
    fixture
        .call(json!({
            "intent": "seed two stale rules",
            "target": "project",
            "operations": [
                { "action": "add", "content": "the package manager is yarn" },
                { "action": "add", "content": "node 18 is the supported runtime" },
            ],
        }))
        .await;

    let consolidated = fixture
        .call(json!({
            "intent": "retire both stale rules and record what replaced them",
            "target": "project",
            "operations": [
                { "action": "remove", "old_text": "yarn" },
                { "action": "replace", "old_text": "node 18",
                  "content": "bun is the runtime; the CI gate is `bun test`" },
            ],
        }))
        .await;
    let consolidated_body = body(&consolidated);

    assert_eq!(consolidated_body["success"], json!(true));
    assert_eq!(consolidated_body["entry_count"], json!(1));
    assert!(
        consolidated_body.get("current_entries").is_none(),
        "success must not echo the entries: {consolidated_body}"
    );
    assert!(
        consolidated_body["usage"]
            .as_str()
            .expect("usage")
            .contains("/3,000 chars"),
        "every response reports current/limit: {consolidated_body}"
    );

    let entries = fixture.store(Scope::Project).entries().to_vec();
    assert_eq!(
        entries,
        vec!["bun is the runtime; the CI gate is `bun test`"]
    );
}

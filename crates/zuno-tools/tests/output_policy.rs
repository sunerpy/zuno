use serde_json::json;
use std::sync::Arc;
use zuno_tool::{
    ACCEPT_LARGE_OUTPUT_KEY, AllowAll, NeverInterrupted, OutputLimits, Tool, ToolContext,
    ToolOutput, ToolOutputStore,
};
use zuno_tools::output_policy::OutputPolicy;
use zuno_tools::shell::ShellTool;

fn limits() -> OutputLimits {
    OutputLimits {
        max_lines: 1,
        max_bytes: 4,
    }
}

fn context() -> ToolContext {
    ToolContext::new(
        "ses_output_policy",
        "msg_output_policy",
        "call_output_policy",
        "build",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
}

#[test]
fn output_policy_refuses_oversized_output_after_persisting_every_byte() {
    let store_dir = tempfile::tempdir().expect("store dir");
    let store = ToolOutputStore::new(store_dir.path());
    let policy = OutputPolicy::new(store.clone(), limits());
    let full = "one\ntwo\n";

    let error = policy
        .apply(
            "shell",
            "ses_output_policy",
            ToolOutput::text("large", full),
            false,
        )
        .expect_err("oversized output requires explicit acceptance");

    let zuno_tools::output_policy::OutputPolicyError::Oversized(refusal) = &error else {
        panic!("expected a structured oversized-output refusal, got {error:?}");
    };
    assert_eq!(refusal.measurement.bytes, full.len());
    assert_eq!(refusal.measurement.lines, 3);
    assert_eq!(
        store
            .read("shell", &refusal.output_path)
            .expect("persisted full output"),
        full
    );

    let message = error.to_string();
    assert!(message.contains("8 bytes across 3 lines"), "{message}");
    assert!(message.contains("~2 tokens"), "{message}");
    assert!(message.contains(ACCEPT_LARGE_OUTPUT_KEY), "{message}");
    assert!(
        message.contains(&refusal.output_path.to_string_lossy().into_owned()),
        "{message}"
    );
}

#[test]
fn output_policy_explicit_acceptance_returns_the_complete_output() {
    let store_dir = tempfile::tempdir().expect("store dir");
    let store = ToolOutputStore::new(store_dir.path());
    let policy = OutputPolicy::new(store.clone(), limits());
    let full = "one\ntwo\n";

    let output = policy
        .apply(
            "shell",
            "ses_output_policy",
            ToolOutput::text("large", full),
            true,
        )
        .expect("explicit acceptance returns the full result");

    assert_eq!(output.output, full);
    assert_eq!(output.metadata["oversized"], true);
    assert_eq!(output.metadata["largeOutputAccepted"], true);
    let paths = output.output_paths();
    let path = paths.first().expect("retrieval path");
    assert_eq!(
        store
            .read("shell", std::path::Path::new(path))
            .expect("persisted full output"),
        full
    );
}

#[test]
fn output_policy_leaves_output_within_limits_unstored_and_unchanged() {
    let store_dir = tempfile::tempdir().expect("store dir");
    let store = ToolOutputStore::new(store_dir.path());
    let policy = OutputPolicy::new(store.clone(), limits());

    let output = policy
        .apply(
            "shell",
            "ses_output_policy",
            ToolOutput::text("small", "four"),
            false,
        )
        .expect("output at the inclusive byte limit fits");

    assert_eq!(output.output, "four");
    assert!(output.output_paths().is_empty());
    assert!(store.entries("shell").expect("store entries").is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn output_policy_shell_reads_the_central_accept_large_output_flag_before_decoding() {
    let workspace = tempfile::tempdir().expect("workspace");
    let store_dir = tempfile::tempdir().expect("store dir");
    let tool = ShellTool::new(workspace.path())
        .expect("shell tool")
        .with_output_store(ToolOutputStore::new(store_dir.path()))
        .with_output_limits(limits());

    let output = tool
        .execute(
            json!({
                "command": "printf 'one\\ntwo\\n'",
                "intent": "verify shell opt-in",
                ACCEPT_LARGE_OUTPUT_KEY: true,
            }),
            context(),
        )
        .await
        .expect("the direct Tool implementation observes the cross-cutting flag");

    assert_eq!(output.output, "one\ntwo\n");
    assert_eq!(output.metadata["largeOutputAccepted"], true);
}

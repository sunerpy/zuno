mod support;

#[cfg(unix)]
use serde_json::json;
#[cfg(unix)]
use std::sync::Arc;
use zuno_tool::{ACCEPT_LARGE_OUTPUT_KEY, OutputLimits, ToolOutput, ToolOutputStore};
#[cfg(unix)]
use zuno_tool::{AllowAll, NeverInterrupted, Tool, ToolContext};
use zuno_tools::output_policy::OutputPolicy;

/// Every window of one artifact, joined, as a caller pages it back.
fn read_all(store: &ToolOutputStore, session_id: &str, path: &std::path::Path) -> String {
    let mut bytes = Vec::new();
    let mut cursor = 0u64;
    loop {
        let window = store
            .read_window("shell", session_id, path, cursor, 4_096)
            .expect("read window");
        bytes.extend_from_slice(&window.bytes);
        cursor = window.cursor;
        if cursor >= window.total {
            return String::from_utf8(bytes).expect("text artifact");
        }
    }
}

fn limits() -> OutputLimits {
    OutputLimits {
        max_lines: 1,
        max_bytes: 4,
    }
}

#[cfg(unix)]
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
        read_all(&store, "ses_output_policy", &refusal.output_path),
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
        read_all(&store, "ses_output_policy", std::path::Path::new(path)),
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
    let tool = support::sandbox::shell_tool(workspace.path())
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

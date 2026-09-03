mod support;

#[cfg(unix)]
use serde_json::json;
#[cfg(unix)]
use std::sync::Arc;
use zuno_tool::{ACCEPT_LARGE_OUTPUT_KEY, OutputLimits, ToolOutput, ToolOutputStore};
#[cfg(unix)]
use zuno_tool::{AllowAll, NeverInterrupted, Tool, ToolContext};
use zuno_tools::output_policy::METADATA_WITHHELD_OUTPUT_KEY;
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

/// Withholding is a successful result that says how to read what was withheld.
///
/// This used to be an error, and the annotated result it dropped on the way out was the
/// only thing that knew where the artifact was. The expectation changed with the
/// behaviour: a withheld result now carries the notice as its text, the artifact on its
/// `outputPaths`, and the typed facts a client can act on without parsing prose.
#[test]
fn withheld_output_is_a_result_that_names_the_artifact_and_the_way_to_read_it() {
    let store_dir = tempfile::tempdir().expect("store dir");
    let store = ToolOutputStore::new(store_dir.path());
    let policy = OutputPolicy::new(store.clone(), limits());
    let full = "one\ntwo\n";

    let output = policy
        .apply(
            "shell",
            "ses_output_policy",
            ToolOutput::text("large", full),
            false,
        )
        .expect("being over the limit is an outcome, not a failure");

    let paths = output.output_paths();
    let path = paths.first().expect("retrieval path");
    assert_eq!(
        read_all(&store, "ses_output_policy", std::path::Path::new(path)),
        full
    );
    assert_eq!(output.metadata["oversized"], true);
    assert!(output.metadata.get("largeOutputAccepted").is_none());

    let facts = &output.metadata[METADATA_WITHHELD_OUTPUT_KEY];
    assert_eq!(facts["outputPath"], *path);
    assert_eq!(facts["artifactBytes"], full.len());
    assert_eq!(facts["artifactLines"], 3);
    assert_eq!(facts["measuredBytes"], full.len());
    assert_eq!(facts["measuredLines"], 3);
    assert_eq!(facts["limitBytes"], 4);
    assert_eq!(facts["limitLines"], 1);
    assert_eq!(facts["estimatedTokens"], 2);
    assert_eq!(facts["retrievalTool"], "bg");
    assert_eq!(facts["retrievalAction"], "artifact");

    let notice = &output.output;
    assert!(notice.contains("8 bytes across 3 lines"), "{notice}");
    assert!(notice.contains("~2 tokens"), "{notice}");
    assert!(notice.contains(*path), "{notice}");
    // Retrieval before the back door: the artifact already exists, while the back door
    // re-runs a call that `shell` declares must never be replayed.
    let retrieval = notice.find("`bg`").expect("retrieval offer");
    let backdoor = notice
        .find(ACCEPT_LARGE_OUTPUT_KEY)
        .expect("inline back door");
    assert!(retrieval < backdoor, "{notice}");
    assert!(notice.contains("run again"), "{notice}");
}

/// The artifact holds the command's bytes, not a decoded copy of them.
#[test]
fn withheld_output_persists_the_bytes_the_command_wrote() {
    let store_dir = tempfile::tempdir().expect("store dir");
    let store = ToolOutputStore::new(store_dir.path());
    let policy = OutputPolicy::new(store.clone(), limits());
    let raw = b"one\n\xff\xfe\ntwo\n";
    let lossy = String::from_utf8_lossy(raw).into_owned();

    let output = policy
        .apply_bytes(
            "shell",
            "ses_output_policy",
            ToolOutput::text("large", &lossy),
            raw,
            false,
        )
        .expect("withheld");

    let paths = output.output_paths();
    let path = paths.first().expect("retrieval path");
    let window = store
        .read_window(
            "shell",
            "ses_output_policy",
            std::path::Path::new(path),
            0,
            4_096,
        )
        .expect("read window");
    assert_eq!(window.bytes, raw);
    // The notice measures what the model would have been shown, which is the decoded
    // text, while the artifact reports its own byte count.
    let facts = &output.metadata[METADATA_WITHHELD_OUTPUT_KEY];
    assert_eq!(facts["artifactBytes"], raw.len());
    assert_eq!(facts["measuredBytes"], lossy.len());
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

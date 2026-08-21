use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("zuno-cli is under <workspace>/crates")
        .to_path_buf()
}

fn read(relative: &str) -> String {
    let path = workspace_root().join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn contains_all(relative: &str, needles: &[&str]) {
    let text = read(relative);
    for needle in needles {
        assert!(text.contains(needle), "{relative} must document {needle:?}");
    }
}

#[test]
fn harness_guide_documents_the_native_extension_contract() {
    contains_all(
        "docs/harness-runtime.md",
        &[
            "Component",
            "ProfileBundle",
            "HarnessProfile",
            "HarnessRuntime",
            "AgentDriver",
            "ToolManifest",
            "ToolContributions",
            "profile_with_tools",
            "transactional",
            "durable inbox",
            "Durable goal recovery",
            "goal_retry",
            "initial_delay_ms",
            "Retry-After",
            "Tool failures do not cause the harness to replay a call",
            "reportDelivery",
            "nextStep",
            "quiet",
            "queries: string[]",
            "WebSearchProvider",
        ],
    );
}

#[test]
fn readmes_point_to_the_native_harness_and_do_not_advertise_opencode_plugins() {
    for relative in ["README.md", "docs/readme/README.en.md"] {
        let text = read(relative);
        assert!(
            text.contains("harness-runtime.md"),
            "{relative} must link the native harness guide"
        );
        for retired in [
            "supports opencode plugins",
            "支持 opencode 插件",
            "zuno-plugin-sdk",
            "plugin_runtime",
            "21 hooks",
        ] {
            assert!(
                !text.contains(retired),
                "{relative} still advertises retired compatibility text {retired:?}"
            );
        }
    }
}

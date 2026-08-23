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
            "Native agents",
            "`build`",
            "`plan`",
            "`deep`",
            "Prompt provenance",
            "session.prompt.assembled",
            "durable inbox",
            "Durable goal recovery",
            "goal_retry",
            "initial_delay_ms",
            "Retry-After",
            "ToolReplayPolicy::Never",
            "authoritative inspection",
            "reportDelivery",
            "nextStep",
            "quiet",
            "ProductAgent",
            "job_cancel",
            "uncertain",
            "queries: string[]",
            "WebSearchProvider",
        ],
    );
}

#[test]
fn plugin_guide_documents_capabilities_protocols_and_examples() {
    contains_all(
        "docs/plugins.md",
        &[
            "zuno plugin add",
            "zuno plugin update",
            "workspace.read",
            "workspace.write",
            "`network`",
            "`environment` names",
            "`host.full`",
            "authorization.strict",
            "subagent_type",
            "memoryMiB",
            "zuno.plugin/1",
            "wit/zuno-plugin/plugin.wit",
            "examples/plugins/review-kit",
            "examples/plugins/wasi-word-count",
            "examples/plugins/process-review",
        ],
    );
}

#[test]
fn architecture_documents_pin_the_native_harness_decisions() {
    contains_all(
        "AGENTS.md",
        &[
            "Everything is a native component",
            "Model-visible means logged",
            "ToolReplayPolicy::Never",
            "reportDelivery: nextStep",
            "$zuno-dsh-sync",
        ],
    );
    contains_all(
        "docs/design/harness-comparison.md",
        &[
            "2026-08-21",
            "dsh-v0.1.1-rc.1",
            "528c682e061696f5a160f363f236ecbf53cbd006",
            "dsh-v0.1.1-rc.2",
            "b150a551b8d465e31e418e1b2eaf5e79bbb7d28e",
            "OpenAI Codex",
            "oh-my-openagent",
            "pi-agent",
            "OpenCode",
            "Claw Code",
            "Cross-project compatibility",
        ],
    );
    contains_all(
        "docs/design/client-interfaces.md",
        &[
            "cursor-based replay",
            "durable inbox",
            "admission identifier",
            "future GUI",
            "A client disconnect never cancels an active goal",
            "GET /api/session/{sessionID}/event",
            "Last-Event-ID",
            "does not mount an unscoped `/event` adapter",
            "only when a real handler exists",
        ],
    );
    contains_all(
        "docs/design/provider-authentication.md",
        &[
            "AuthStore",
            "LoginMethodRegistry",
            "chatgpt-browser",
            "chatgpt-device",
            "ChatGPT-Account-Id",
            "ZUNO_AUTH_CONTENT",
            "transport",
            "myopenai",
        ],
    );
    contains_all(
        "docs/design/product-agents.md",
        &[
            "productAgent",
            "subagent_codex",
            "subagent_claude_code",
            "app-server",
            "stream-json",
            "ToolReplayPolicy::Never",
            "JobSubject",
            "uncertain",
            "job_cancel",
            "ToolUiIntent::Subagent",
        ],
    );
}

#[test]
fn readmes_document_extension_examples_and_do_not_advertise_compatibility() {
    for relative in ["README.md", "docs/readme/README.en.md"] {
        let text = read(relative);
        assert!(
            text.contains("harness-runtime.md"),
            "{relative} must link the native harness guide"
        );
        for required in [
            "profile_with_tools",
            "AgentDriver",
            "session.prompt.assembled",
            "design/harness-comparison.md",
            "design/client-interfaces.md",
            "plugins.md",
        ] {
            assert!(
                text.contains(required),
                "{relative} must document extension surface {required:?}"
            );
        }
        for retired in [
            "supports opencode plugins",
            "支持 opencode 插件",
            "zuno-plugin-sdk",
            "plugin_runtime",
            "21 hooks",
            "rejected-inputs.md",
            "legacy-filename diagnostics",
            "旧默认文件名诊断",
        ] {
            assert!(
                !text.contains(retired),
                "{relative} still advertises retired compatibility text {retired:?}"
            );
        }
    }
}

#[test]
fn provider_setup_recommends_native_transports_without_node_bootstrap() {
    for relative in [
        "README.md",
        "docs/readme/README.en.md",
        "docs/reference/configuration.md",
        "docs/reference/providers.md",
        "crates/zuno-catalog/src/skill/customize-zuno.md",
        "examples/config/zuno.json",
    ] {
        let text = read(relative);
        assert!(
            text.contains("myopenai"),
            "{relative} must use the checked custom provider id"
        );
        assert!(
            text.contains("transport"),
            "{relative} must name the native provider selector"
        );
        for retired in [r#""npm":"#, "@ai-sdk/", r#""npx""#] {
            assert!(
                !text.contains(retired),
                "{relative} contains retired provider bootstrap form {retired:?}"
            );
        }
    }

    for relative in [
        "README.md",
        "docs/readme/README.en.md",
        "docs/reference/providers.md",
        "crates/zuno-catalog/src/skill/customize-zuno.md",
    ] {
        contains_all(
            relative,
            &[
                "zuno providers login --provider myopenai",
                "zuno debug config",
                "zuno models myopenai --verbose",
            ],
        );
    }

    contains_all(
        "examples/config/zuno.json",
        &[
            r#""transport": "openai""#,
            r#""model": "myopenai/primary-model""#,
            r#""small_model": "myopenai/fast-model""#,
        ],
    );
    contains_all(
        "docs/reference/providers.md",
        &[
            "zuno auth methods openai",
            "zuno auth login openai --method chatgpt-device",
            "zuno auth login openai --method api-key",
            "first non-empty variable",
            "not copied into `auth.json`",
        ],
    );
}

#[test]
fn database_docs_describe_a_hard_pre_release_format_cut() {
    let text = read("docs/migration.md");
    for required in [
        "unsupported pre-release format",
        "without modification",
        "no incremental database migration",
        "zuno_schema",
        "never deletes or rewrites",
    ] {
        assert!(
            text.contains(required),
            "migration guide must contain {required:?}"
        );
    }
    for retired in [
        "Pre-rename Zuno database filename",
        "__drizzle_migrations",
        "opencode.db",
        "rejected-inputs.md",
    ] {
        assert!(
            !text.contains(retired),
            "migration guide still advertises retired migration surface {retired:?}"
        );
    }
}

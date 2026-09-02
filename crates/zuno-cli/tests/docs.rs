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
fn session_retention_table_list_tracks_the_destructive_delete_order() {
    let text = read("docs/session-retention.md");
    let begin = text
        .find("<!-- generated:BEGIN prune-tables -->")
        .expect("retention guide has a generated table start");
    let end = text
        .find("<!-- generated:END prune-tables -->")
        .expect("retention guide has a generated table end");
    let block = &text[begin..end];
    assert!(
        text.contains(&format!(
            "**{} tables**, in this order:",
            zuno_db::prune::DELETE_ORDER.len()
        )),
        "retention guide table count drifted from DELETE_ORDER"
    );
    for (index, table) in zuno_db::prune::DELETE_ORDER.iter().enumerate() {
        let row = format!("| {} | `{table}` |", index + 1);
        assert!(block.contains(&row), "retention guide is missing {row}");
    }
    assert_eq!(
        block
            .lines()
            .filter(|line| line.starts_with("| ") && line.contains('`'))
            .count(),
        zuno_db::prune::DELETE_ORDER.len(),
        "retention guide contains an extra or stale table row"
    );
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
            "Provider request routing context",
            "ProviderRequestContext",
            "metadata.zuno_session_id",
            "requestPurpose",
            "affinityAttached",
            "affinitySource",
            "durable inbox",
            "`Ctrl+Enter`",
            "`Shift+Enter`",
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
            "typed rich content",
            "[Image #N]",
            "unsupported typed input",
        ],
    );
}

#[test]
fn sandbox_docs_pin_the_trusted_unavailable_fallback_contract() {
    contains_all(
        "docs/design/shell-sandbox-roadmap.md",
        &[
            "sandbox.onUnavailable",
            "UnavailableFallback",
            "read-only Agent contracts",
            "never fall back",
            "command preparation/execution failure",
        ],
    );
    contains_all(
        "docs/harness-runtime.md",
        &[
            "`runtime.sandbox`",
            "`run-unconfined`",
            "requestedMode",
            "fallbackReason",
            "Version-2 background records",
        ],
    );
    contains_all(
        "docs/reference/configuration.md",
        &[
            "\"onUnavailable\": \"deny\"",
            "`ZUNO_SANDBOX_ON_UNAVAILABLE=run-unconfined`",
            "A read-only Agent never runs unconfined",
        ],
    );
    contains_all(
        "docs/faq.md",
        &[
            "`sandbox.onUnavailable`",
            "fallback eligibility",
            "`--check` exits unsuccessfully",
        ],
    );
    contains_all(
        "docs/guide/permissions.md",
        &[
            "Choosing native execution",
            "\"onUnavailable\": \"run-unconfined\"",
            "ZUNO_SANDBOX_ON_UNAVAILABLE=run-unconfined",
            "read-only Agent never uses",
        ],
    );
    contains_all(
        "docs/zh/guide/permissions.md",
        &[
            "如何选择无沙箱执行",
            "\"onUnavailable\": \"run-unconfined\"",
            "ZUNO_SANDBOX_ON_UNAVAILABLE=run-unconfined",
            "只读 Agent 永远不会使用",
        ],
    );
    contains_all(
        "docs/config/index.md",
        &[
            "Choosing no-sandbox behavior",
            "\"mode\": \"danger-full-access\"",
            "\"onUnavailable\": \"run-unconfined\"",
        ],
    );
    contains_all(
        "docs/zh/config/index.md",
        &[
            "选择无沙箱行为",
            "\"mode\": \"danger-full-access\"",
            "\"onUnavailable\": \"run-unconfined\"",
        ],
    );
    for relative in [
        "docs/operate/diagnostics.md",
        "docs/zh/operate/diagnostics.md",
    ] {
        contains_all(
            relative,
            &[
                "--sandbox-on-unavailable",
                "requestedMode",
                "unavailable_fallback",
            ],
        );
    }
    contains_all(
        "docs/zh/config/reference.md",
        &[
            "沙箱模式与后端不可用策略",
            "ZUNO_SANDBOX_ON_UNAVAILABLE=run-unconfined",
            "fallbackReason",
        ],
    );
    contains_all(
        "docs/zh/operate/faq.md",
        &[
            "`sandbox.onUnavailable`",
            "run-unconfined",
            "`debug sandbox --check`",
        ],
    );
    for relative in [
        "docs/cli/global-options.md",
        "docs/cli/debug.md",
        "docs/zh/cli/global-options.md",
        "docs/zh/cli/debug.md",
    ] {
        contains_all(relative, &["--sandbox-on-unavailable", "run-unconfined"]);
    }

    for directory in ["docs/cli", "docs/zh/cli"] {
        let path = workspace_root().join(directory);
        for entry in std::fs::read_dir(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        {
            let path = entry.expect("read CLI docs entry").path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("md") {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            let sandbox_options = text.matches("--sandbox <SANDBOX>").count();
            let unavailable_options = text.matches("--sandbox-on-unavailable <ACTION>").count();
            assert_eq!(
                unavailable_options,
                sandbox_options,
                "{} must keep the sandbox global options together",
                path.display()
            );
        }
    }
}

#[test]
fn continuity_docs_explain_switching_scope_and_final_tool_filters() {
    contains_all(
        "docs/config/continuity.md",
        &[
            "\"continuity\": true",
            "\"history\": false",
            "\"notes\": true",
            "ZUNO_CONFIG_DIR",
            "ZUNO_CONFIG_CONTENT",
            "zuno acp",
            "restart a long-running TUI, ACP server, or HTTP server",
            "top-level `tools` map",
            "`permission.rules`",
            "expected_revision",
            "session_id + Agent",
            "`runtime.continuity`",
            "\"plan_update\"",
        ],
    );
    contains_all(
        "docs/zh/config/continuity.md",
        &[
            "\"continuity\": true",
            "\"history\": false",
            "\"notes\": true",
            "ZUNO_CONFIG_DIR",
            "ZUNO_CONFIG_CONTENT",
            "zuno acp",
            "重启长期运行的 TUI",
            "顶层 `tools` 映射",
            "`permission.rules`",
            "expected_revision",
            "session_id + Agent",
            "`runtime.continuity`",
            "\"plan_update\"",
        ],
    );
    contains_all(
        "docs/config/index.md",
        &["[History and Notes continuity](/config/continuity)"],
    );
    contains_all(
        "docs/zh/config/index.md",
        &["[History 与 Notes 连续性配置](/zh/config/continuity)"],
    );
}

#[test]
fn portable_bundle_and_attachment_guides_document_the_public_contracts() {
    contains_all(
        "docs/reference/portable-bundles.md",
        &[
            "zuno export",
            "zuno import",
            ".zuno-bundle",
            "AGENTS.md",
            "--include-credentials",
            "--dry-run",
            "--replace",
            "SHA-256",
            "Windows reserved device names",
            "session databases",
        ],
    );
    contains_all(
        "docs/reference/attachments.md",
        &[
            "[Image #1]",
            "image/png",
            "image/jpeg",
            "image/gif",
            "image/webp",
            "20 MiB",
            "@src/main.rs",
            "51,200 bytes",
            "zuno run -f/--file",
            "unsupported_capability",
            "durable file part",
            "ImageAttachmentRef",
            "max_encoded_bytes",
            "database-identity",
            "do not contain base64",
        ],
    );
    for relative in ["README.md", "docs/readme/README.zh-CN.md", "docs/README.md"] {
        contains_all(
            relative,
            &["reference/portable-bundles.md", "reference/attachments.md"],
        );
    }
}

#[test]
fn zed_acp_guide_documents_cross_platform_setup_and_agent_selection() {
    contains_all(
        "docs/reference/zed-acp.md",
        &[
            "zuno acp --check",
            "agent_servers",
            r#""args": ["acp"]"#,
            "command -v zuno",
            "Get-Command zuno",
            r#""C:\\Users\\you\\.local\\bin\\zuno.exe""#,
            "ZUNO_CONFIG_DIR",
            "Agent",
            "`deep`",
            "`/goal`",
            "`/plan`",
            "`/start-plan`",
            "`/start-work`",
            "Streamable HTTP",
            "complete list",
            "never stored in SQLite or logs",
            "dev: open acp logs",
            "stdout",
            "cargo test -p zuno --test acp_stdio",
            "https://zed.dev/docs/ai/external-agents",
        ],
    );
    for relative in ["README.md", "docs/readme/README.zh-CN.md", "docs/README.md"] {
        contains_all(relative, &["reference/zed-acp.md"]);
    }
    contains_all(
        "docs/design/zed-acp-integration.md",
        &["../reference/zed-acp.md"],
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
            "permission.mode",
            "\"agent\": \"release-reviewer\"",
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
fn extension_development_docs_pin_supported_boundaries_and_ownership() {
    contains_all(
        "docs/guide/extension-development.md",
        &[
            "zuno.extension/v1",
            "zuno.plugin/1",
            "wasm32-wasip2",
            "wit-bindgen",
            "workspace.read",
            "host.full",
            "Component::prepare",
            "PrepareContext",
            "ProfileBundle",
            "HarnessProfile",
            "AgentDriver",
            "ToolReplayPolicy",
            "Uncertain",
            "scripts/check-plugin-examples.sh",
            "crates/zuno-extension/src/host/wasi.rs",
        ],
    );
    contains_all(
        "docs/zh/guide/extension-development.md",
        &[
            "zuno.extension/v1",
            "zuno.plugin/1",
            "wasm32-wasip2",
            "Component::prepare",
            "ProfileBundle",
            "HarnessProfile",
            "AgentDriver",
            "Uncertain",
            "文档架构与覆盖地图",
        ],
    );
    for relative in [
        "docs/design/documentation-coverage.md",
        "docs/zh/design/documentation-coverage.md",
    ] {
        contains_all(
            relative,
            &[
                ".github/workflows/publish-docs.yml",
                "docs/scripts/sync-zuno-docs.sh",
                "zuno.firlab.app",
                "cargo test -p zuno --test docs",
            ],
        );
    }
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
            "A database format shipped in a release is durable user state",
            "atomic forward migration",
            "marker updated last",
            "Future, unmarked, or structurally corrupt formats fail closed",
            "Cross-Platform Development",
            "backend dependency of the `glob` and `grep` tools only",
            "Cross-compilation is useful evidence but does not replace native execution",
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
            "dsh-v0.1.2-alpha.2",
            "0a53fb55bea101816fa226bb964ae2bed71c343b",
            "alpha.2 adoption ledger",
            "OpenAI Codex",
            "oh-my-openagent",
            "pi-agent",
            "OpenCode",
            "Claw Code",
            "Cross-project compatibility",
        ],
    );
    contains_all(
        "docs/design/dsh-alpha2-adoption-ledger.md",
        &[
            "1,313 commits",
            "6,808 changed files",
            "No unclassified path group remains",
            "Public web fetch target validation",
            "ACP session-provided MCP",
            "Loopback browser authentication",
            "Provider Files API fallback",
            "reject",
            "watch",
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
    for relative in ["README.md", "docs/readme/README.zh-CN.md"] {
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
            "guide/extension-development.md",
            "design/documentation-coverage.md",
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
        "docs/readme/README.zh-CN.md",
        "docs/reference/configuration.md",
        "docs/reference/providers.md",
        "crates/zuno-orchestration/src/skills/customize-zuno.md",
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
        "docs/readme/README.zh-CN.md",
        "docs/reference/providers.md",
        "crates/zuno-orchestration/src/skills/customize-zuno.md",
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
            "metadata.zuno_session_id",
            "durable root or child session identity",
            "title, summary, compaction, learning extraction, and Council calls are isolated",
        ],
    );
    contains_all(
        "docs/reference/providers.md",
        &[
            r#""headers": {"X-Tenant": "tenant-a"}"#,
            "Provider-level headers are defaults for every configured model",
            "model-level `headers` win",
            "`Authorization`, `Content-Type`, and `Accept`",
        ],
    );
    contains_all(
        "docs/reference/configuration.md",
        &[
            "`legacy-user-prefix` changes instruction projection only",
            "enable_legacy_chat_completions: false",
            "`previous_response_id`",
            "`store: true`",
            "`input_file` support",
            "remote image URLs",
            "one long-lived kiro-provider process",
        ],
    );
}

#[test]
fn multi_provider_example_routes_only_zuno_agents() {
    let relative = "examples/config/zuno-multi-provider.json";
    let value: serde_json::Value =
        serde_json::from_str(&read(relative)).expect("multi-provider example is valid JSON");

    let providers = value["provider"]
        .as_object()
        .expect("multi-provider example declares providers");
    assert_eq!(
        providers.keys().map(String::as_str).collect::<Vec<_>>(),
        ["kiro-local", "myopenai"],
        "the checked example should keep both providers in one config"
    );
    assert!(
        providers["myopenai"]["models"]
            .get("us.anthropic.claude-fable-5")
            .is_some(),
        "the myopenai catalog must include Claude Fable 5"
    );
    for model in [
        "claude-opus-5",
        "claude-opus-4-8",
        "claude-sonnet-5",
        "gpt-5.6-sol",
        "gpt-5.6-luna",
    ] {
        assert!(
            providers["kiro-local"]["models"].get(model).is_some(),
            "the Kiro catalog must include {model}"
        );
    }
    assert_eq!(
        providers["kiro-local"]["options"]["maxTokens"],
        serde_json::Value::Null,
        "Zuno must not inject a generic output cap into Kiro Responses requests"
    );
    assert!(
        providers["kiro-local"]["options"]
            .get("responsesTextBlocks")
            .is_none(),
        "current kiro-provider preserves consecutive text blocks itself; Zuno's single-text compatibility projection would insert a blank line"
    );
    assert_eq!(
        providers["kiro-local"]["models"]["claude-opus-5"]["limit"]["context"],
        1_000_000
    );
    assert_eq!(
        providers["kiro-local"]["models"]["claude-opus-5"]["limit"]["output"],
        128_000
    );
    for (model, definition) in providers["kiro-local"]["models"]
        .as_object()
        .expect("Kiro models are an object")
    {
        assert!(
            definition["options"].get("reasoningSummary").is_none(),
            "{model} requests reasoning.summary even though kiro-provider rejects that field"
        );
        let input = definition["modalities"]["input"]
            .as_array()
            .expect("every Kiro model declares its accepted input subset");
        assert!(
            !input.iter().any(|modality| modality == "pdf"),
            "{model} advertises PDF before Zuno has a native document request block and an end-to-end Kiro document test"
        );
    }

    let expected_agents = [
        "build",
        "deep",
        "explorer",
        "fixer",
        "general",
        "librarian",
        "looker",
        "oracle",
        "orchestrator",
        "plan",
    ];
    let presets = value["presets"]
        .as_object()
        .expect("multi-provider example declares presets");
    assert_eq!(
        presets.keys().map(String::as_str).collect::<Vec<_>>(),
        ["hybrid", "kiro-local", "myopenai"]
    );
    for (name, preset) in presets {
        let agents = preset["agents"]
            .as_object()
            .unwrap_or_else(|| panic!("preset {name} declares Agent routes"));
        let mut actual = agents.keys().map(String::as_str).collect::<Vec<_>>();
        actual.sort_unstable();
        assert_eq!(
            actual, expected_agents,
            "preset {name} must route the complete Zuno user-Agent roster"
        );
        assert!(
            preset.get("categories").is_none(),
            "OMO categories must not be copied into Zuno presets"
        );
    }
    assert_eq!(
        presets["kiro-local"]["agents"]["deep"]["model"],
        "kiro-local/claude-opus-5"
    );

    let text = read(relative);
    for foreign in [
        "sisyphus",
        "hephaestus",
        "prometheus",
        "metis",
        "momus",
        "atlas",
        "ultrabrain",
        "visual-engineering",
        "unspecified-low",
    ] {
        assert!(
            !text.contains(foreign),
            "multi-provider example copied foreign OMO identity {foreign:?}"
        );
    }

    contains_all(
        "docs/reference/configuration.md",
        &[
            "examples/config/zuno-multi-provider.json",
            "`myopenai`",
            "`kiro-local`",
            "`hybrid`",
            "claude-opus-5",
            "ZUNO_CONFIG_DIR",
            "/preset",
            "byte-for-byte with no inserted separator",
            "Do not set `reasoningSummary`",
        ],
    );
}

#[test]
fn self_update_documentation_pins_the_verified_release_contract() {
    contains_all(
        "docs/reference/self-update.md",
        &[
            "zuno self-update --check",
            "`--tag`",
            "`--force`",
            "`--yes`",
            "x86_64-unknown-linux-musl",
            "SHA256SUMS",
            "atomic self-replace",
            "GITHUB_TOKEN",
            "GH_TOKEN",
            "HTTPS_PROXY",
            "NO_PROXY",
        ],
    );
    for relative in ["README.md", "docs/readme/README.zh-CN.md", "docs/README.md"] {
        contains_all(relative, &["self-update", "reference/self-update.md"]);
    }
}

#[test]
fn installation_docs_pin_cross_platform_dependency_boundaries() {
    contains_all(
        "docs/guide/installation.md",
        &[
            "not a Zuno startup or core-runtime dependency",
            "Linux-only backend",
            "`read-only`",
            "`workspace-write`",
            "`danger-full-access`",
            "`run-unconfined`",
            "macOS",
            "Windows PowerShell",
            "Get-FileHash",
            "ZUNO_CONFIG_DIR",
            "Rust 1.98.0",
            "Xcode Command Line Tools",
            "MSVC v143",
        ],
    );
    contains_all(
        "docs/zh/guide/installation.md",
        &[
            "不是 Zuno 启动或核心运行依赖",
            "只作为 Linux",
            "`read-only`",
            "`workspace-write`",
            "`danger-full-access`",
            "`run-unconfined`",
            "macOS",
            "Windows PowerShell",
            "Get-FileHash",
            "ZUNO_CONFIG_DIR",
            "Rust 1.98.0",
            "Xcode Command Line Tools",
            "MSVC v143",
        ],
    );
    for relative in [
        "README.md",
        "docs/index.md",
        "docs/guide/installation.md",
        "docs/guide/quick-start.md",
        "docs/zh/index.md",
        "docs/zh/guide/installation.md",
        "docs/zh/guide/quick-start.md",
    ] {
        let text = read(relative);
        for required in [
            "`glob`",
            "`grep`",
            "`danger-full-access`",
            "`workspace-write`",
            "`run-unconfined`",
            "macOS",
            "Windows",
        ] {
            assert!(
                text.contains(required),
                "{relative} must document {required:?}"
            );
        }
        assert!(
            !text.contains("0.0.1"),
            "{relative} still advertises the retired 0.0.1 release"
        );
    }
}

#[test]
fn installation_docs_use_release_placeholders_instead_of_stale_version_pins() {
    for (relative, explanation) in [
        (
            "docs/guide/installation.md",
            "Replace `X.Y.Z` with the exact published release",
        ),
        (
            "docs/zh/guide/installation.md",
            "将 `X.Y.Z` 替换为准备安装的确切已发布版本",
        ),
    ] {
        let text = read(relative);
        assert!(
            text.contains(explanation),
            "{relative} must explain how to replace its release placeholder"
        );

        for (prefix, expected) in [
            ("ZUNO_VERSION=v", "ZUNO_VERSION=vX.Y.Z \\"),
            ("$env:ZUNO_VERSION = ", "$env:ZUNO_VERSION = \"vX.Y.Z\""),
            ("version=", "version=X.Y.Z"),
            ("$version = ", "$version = \"X.Y.Z\""),
            ("zuno self-update --tag v", "zuno self-update --tag vX.Y.Z"),
        ] {
            let matches = text
                .lines()
                .map(str::trim)
                .filter(|line| line.starts_with(prefix))
                .collect::<Vec<_>>();
            assert_eq!(
                matches,
                vec![expected],
                "{relative} must keep {prefix:?} version-agnostic so a later release cannot \
                 leave an older install command online"
            );
        }
    }
}

#[test]
fn completion_docs_describe_stdout_and_profile_safe_installation() {
    contains_all(
        "docs/cli/completion.md",
        &[
            "`--install`",
            "atomically writes",
            "never edits a shell profile",
            "bash-completion/completions/zuno",
            ".zsh/completions/_zuno",
            "fish/completions/zuno.fish",
            "LOCALAPPDATA",
            "elvish/lib/zuno.elv",
        ],
    );
    contains_all(
        "docs/zh/cli/completion.md",
        &[
            "`--install`",
            "原子写入",
            "绝不会",
            "bash-completion/completions/zuno",
            ".zsh/completions/_zuno",
            "fish/completions/zuno.fish",
            "LOCALAPPDATA",
            "elvish/lib/zuno.elv",
        ],
    );
}

#[test]
fn database_docs_describe_the_guarded_chain_to_the_current_format() {
    contains_all(
        "docs/migration.md",
        &[
            "current database format is 8",
            "Format 5",
            "Format 6",
            "Format 7",
            "`BEGIN IMMEDIATE`",
            "marker from 5, 6, or 7 to 8",
            "`session`, `message`, `memory_candidate`, or `work_plan` values",
            "future format",
            "fails closed without modification",
            "format marker updated last",
            "A valid format-5, format-6, or format-7 database",
            "should open and migrate automatically",
        ],
    );
    contains_all(
        "docs/zh/operate/migration.md",
        &[
            "当前数据库格式为 8",
            "format 5",
            "format 6",
            "format 7",
            "`BEGIN IMMEDIATE`",
            "marker 从 5、6 或 7 改为 8",
            "`session`、`message`、",
            "`work_plan` 值",
            "未来格式",
            "失败关闭且不修改文件",
            "最后更新格式 marker",
            "当前二进制已经支持的格式重建数据库",
        ],
    );
    contains_all(
        "docs/zh/operate/prompt-workflow.md",
        &[
            "数据库当前格式为 8",
            "format 5",
            "format 6",
            "format 7",
            "`BEGIN IMMEDIATE`",
            "`session`",
            "`message`",
            "`memory_candidate`",
            "`work_plan`",
            "不要求重建数据库",
        ],
    );
    let prompt_workflow = read("docs/zh/operate/prompt-workflow.md");
    assert!(
        !prompt_workflow.contains("pre-release format"),
        "prompt workflow still describes a released database format as pre-release"
    );
    for relative in ["docs/migration.md", "docs/zh/operate/migration.md"] {
        let text = read(relative);
        for retired in [
            "no incremental database migration",
            "never upgraded through an incremental migration chain",
            "永远不会通过增量迁移链升级",
            "开发数据库随之重建",
        ] {
            assert!(
                !text.contains(retired),
                "{relative} still advertises retired migration policy {retired:?}"
            );
        }
    }
    let text = read("docs/migration.md");
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

/// Both instruction guides must state that an inadmissible rule file stops the turn.
///
/// The behaviour is the opposite of what it was — a dropped file used to be a warning
/// the turn survived — so a stale guide here does not merely omit a detail. It tells a
/// user their oversized `AGENTS.md` is silently ignored, which is the exact belief the
/// change exists to correct, and the English and Chinese pages must not disagree about
/// which of the two outcomes they will get.
#[test]
fn instruction_guides_document_the_fail_closed_admission() {
    contains_all(
        "docs/config/instructions.md",
        &[
            "## When a rule file stops the turn",
            "admitted whole or not at all",
            "fail the turn before the first provider request",
            "cannot be read",
            "smaller of 64 KB",
            "quarter of the model's context window",
            "Neither is a warning",
            "failed remote fetch is the documented",
        ],
    );
    contains_all(
        "docs/zh/config/instructions.md",
        &[
            "## 什么情况下规则文件会中止本轮",
            "要么整份进入 Prompt，要么完全不进入",
            "第一次 provider request 之前让本轮失败",
            "无法读取",
            "64 KB 与模型 context window 四分之一",
            "两者都不是警告",
            "远端抓取失败是上文记录的例外",
        ],
    );
}

#[test]
fn durable_state_guides_document_evidence_gated_completion() {
    contains_all(
        "docs/guide/durable-state.md",
        &[
            "### Success criteria and evidence",
            "cannot be completed on assertion alone",
            "`satisfy_criteria`",
            "`waive_criteria`",
            "[verification rcp_",
            "Cite this id as evidence",
            "inferred rather than observed",
            "Evidence expires",
            "[goal evidence]",
            "turns a question goal into a change goal",
            "`.git/info/exclude`",
            "### Token budget",
            "around every provider request inside a turn",
            "`turn_budget`",
            "last tenth of the allowance",
        ],
    );
    contains_all(
        "docs/zh/guide/durable-state.md",
        &[
            "### 成功标准与证据",
            "不能仅凭断言完成",
            "`satisfy_criteria`",
            "`waive_criteria`",
            "[verification rcp_",
            "推断得来、而非直接观测到的",
            "证据会过期",
            "[goal evidence]",
            "转成 change Goal",
            "`.git/info/exclude`",
            "### Token 预算",
            "每一次 provider request 前后执行",
            "`turn_budget`",
            "最后十分之一是刻意留出的",
        ],
    );
}

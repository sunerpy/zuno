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
            "dev: open acp logs",
            "stdout",
            "cargo test -p zuno-cli --test acp_stdio",
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
            "title, summary, compaction, reflection, and Council calls are isolated",
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
    assert_eq!(
        providers["kiro-local"]["models"]["claude-opus-5"]["limit"]["context"],
        1_000_000
    );
    assert_eq!(
        providers["kiro-local"]["models"]["claude-opus-5"]["limit"]["output"],
        128_000
    );

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

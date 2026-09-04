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
fn retention_guide_states_that_archiving_withdraws_standing_http_authorizations() {
    // Archiving is reversible in the database, yet it withdraws every standing `always`
    // authorization the selected sessions granted and `restore_archive` does not bring one
    // back. Both retention pages must say so beside their "nothing is removed" claim.
    contains_all(
        "docs/session-retention.md",
        &[
            "Archiving ends a session's standing HTTP authorizations",
            "does not outlive the session",
            "reinstate an authorization",
            "the CLI withdraws nothing",
        ],
    );
    contains_all(
        "docs/zh/operate/session-retention.md",
        &[
            "归档会终止该 session 的常驻 HTTP 授权",
            "不会比给出它的那个 session 活得更久",
            "重新装回一条授权",
            "不持有 request broker",
        ],
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
            "Encrypted reasoning replay",
            "reasoningReplay",
            "reasoning.encrypted_content",
            "replayedReasoningCapsules",
            "withheldReasoningCapsules",
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
fn reconciliation_docs_pin_durable_work_as_the_only_unreconciled_work() {
    contains_all(
        "docs/harness-runtime.md",
        &[
            "A short single-clause question stays atomic whether or not it ends in",
            "a session that recorded no durable work finishes on its first answer",
            "holding unreconciled durable work receives at most two",
            "Unreconciled work means durably recorded work.",
            "recorded no Plan, Todo, or Job is settled",
        ],
    );
    contains_all(
        "docs/zh/operate/harness-runtime.md",
        &[
            "单句短问句",
            "没有记录任何持久工作的会话在第一次回复后直接结束",
            "普通会话在持有未对账的持久工作时",
            "只是宿主的分类预测",
        ],
    );
    contains_all(
        "docs/guide/tools.md",
        &[
            "unreconciled durable work receive at most two reconciliation continuations",
            "durably recorded work counts: a session that recorded no Plan, Todo, or Job",
        ],
    );
    contains_all(
        "docs/zh/guide/tools.md",
        &[
            "普通会话在持有未对账的持久工作时最多执行两次对账续跑",
            "没有记录任何 Plan、Todo 或 Job 的会话在第一次回复后就结束",
        ],
    );
    contains_all(
        "docs/guide/durable-state.md",
        &["a short single-clause question is a direct answer"],
    );
    contains_all(
        "docs/zh/guide/durable-state.md",
        &["单句短问句无论是否带问号都算直接回答"],
    );
    contains_all(
        "docs/zh/operate/prompt-workflow.md",
        &["单句短问句无论是否带问号都属于直接回答"],
    );
    contains_all(
        "docs/design/dsh-alpha2-adoption-ledger.md",
        &[
            "for durably recorded work",
            "A session that recorded no Plan, Todo, or Job is settled, not continued.",
        ],
    );
    for (relative, retired) in [
        (
            "docs/harness-runtime.md",
            "an ordinary session receives at most two durable reconciliation",
        ),
        ("docs/guide/tools.md", "Ordinary sessions receive at"),
        ("docs/zh/guide/tools.md", "普通会话最多执行两次对账续跑"),
        (
            "docs/zh/operate/harness-runtime.md",
            "普通会话最多续跑两次对账",
        ),
    ] {
        assert!(
            !read(relative).contains(retired),
            "{relative} still claims unconditional reconciliation continuations {retired:?}"
        );
    }
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
            // A saved `always` is session-scoped, in memory, and unaffected by a
            // dropped stream. All three are easy to re-document as global, which is
            // what the batch-2 fix stopped being true.
            "belongs to one session",
            "not in the database",
            "a stream is not the session",
        ],
    );
    contains_all(
        "docs/zh/guide/permissions.md",
        &[
            "如何选择无沙箱执行",
            "\"onUnavailable\": \"run-unconfined\"",
            "ZUNO_SANDBOX_ON_UNAVAILABLE=run-unconfined",
            "只读 Agent 永远不会使用",
            "只属于一个 session",
            "而不在数据库里",
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

/// What a macOS or Windows user reads about a host with no confined backend.
///
/// Three separate claims were wrong in the released text, and each is pinned here: the
/// permission guides and quick-start rows presented the bare one-line
/// `OS sandbox is not implemented for platform` as the whole report; the prompt's
/// condition was documented as standard input alone when `is_interactive` requires
/// standard error too; and the interactive answer was documented as identical to
/// `--sandbox-on-unavailable run-unconfined`, which it is only for this process — on
/// Unix the flag reaches child processes through the startup re-exec and the answer,
/// resolved after it, does not.
#[test]
fn unsupported_platform_docs_pin_the_refusal_the_prompt_and_its_process_scope() {
    // The opening clause is quoted on both pages so the string a user greps for still
    // lands where the remedies are, without the page claiming it is the whole report.
    let opening =
        "OS sandbox is not implemented for platform `macos`: macos has no confined sandbox";
    contains_all(
        "docs/guide/permissions.md",
        &[
            "confined backend at all",
            opening,
            "standard error are both terminals",
            "resolves this process",
            "The answer belongs to this process",
            "keeps the current Agent",
        ],
    );
    contains_all(
        "docs/zh/guide/permissions.md",
        &[
            "根本没有受约束后端",
            opening,
            "标准错误都是终端",
            "这个回答只属于当前这个进程",
            "保留当前 Agent",
        ],
    );
    contains_all(
        "docs/reference/configuration.md",
        &[
            "standard input **and** standard error are both",
            "resolves this process exactly as",
            "The answer is not inherited by child processes",
            "no such re-exec",
        ],
    );
    contains_all(
        "docs/zh/config/reference.md",
        &[
            "标准错误都是终端",
            "这个回答不会被子进程继承",
            "没有这一次 re-exec",
        ],
    );
    contains_all(
        "docs/faq.md",
        &[
            "standard input and standard error are both terminals",
            "process exactly as the flag does",
            "startup re-exec that exports it",
        ],
    );
    contains_all(
        "docs/zh/operate/faq.md",
        &[
            "标准错误都是终端",
            "这个回答只对当前",
            "通过启动时的 re-exec 写入真实环境变量",
        ],
    );
    for relative in ["docs/guide/quick-start.md", "docs/zh/guide/quick-start.md"] {
        contains_all(
            relative,
            &["Run this session natively without OS confinement?"],
        );
    }

    // The framing that presented a bare one-line error as the whole report.
    assert!(
        !read("docs/guide/permissions.md").contains("restricted mode reports:"),
        "docs/guide/permissions.md still presents the bare unsupported-platform line"
    );
    assert!(
        !read("docs/zh/guide/permissions.md").contains("受限模式报告："),
        "docs/zh/guide/permissions.md still presents the bare unsupported-platform line"
    );
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
            "scoped to the session that granted",
            "a stream is not the session",
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
            "`reasoningReplay`",
            "`reasoningReplayMaxAge`",
            "reasoning.encrypted_content",
            // The routing rule, in the shape the validator actually enforces: the
            // catalog `openai` provider needs no declaration, a gateway with its own
            // endpoint needs both, and a model's override is checked per model.
            "needs no declaration at all",
            "must declare both",
            "checked per model",
            // Both spellings of the event that carries the envelope, because they
            // differ per client surface: `zuno run --json` prints the snake_case one.
            "provider.reasoning.item",
            "provider_reasoning_item",
            "encryptedContent",
        ],
    );
    contains_all(
        "docs/zh/config/providers.md",
        &[
            "`reasoningReplay`",
            "`reasoningReplayMaxAge`",
            "reasoning.encrypted_content",
            "provider.reasoning.item",
            "provider_reasoning_item",
            "encryptedContent",
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
            "`reasoningReplay`",
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
        providers["kiro-local"]["options"]["reasoningReplay"], "encrypted",
        "the documented Kiro preset must opt into sealed reasoning replay; without it the gateway returns unsealed reasoning that no later turn can replay"
    );
    assert_eq!(
        providers["kiro-local"]["options"]["reasoningReplayMaxAge"], 86_400_000,
        "the age limit must match the gateway's own 24-hour envelope validity"
    );
    assert_eq!(
        (
            providers["kiro-local"]["transport"].as_str(),
            providers["kiro-local"]["surface"].as_str(),
        ),
        (Some("openai"), Some("responses")),
        "encrypted replay is refused at config time without both declarations, because only this pair resolves to a Responses request"
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

    let design = read("docs/design/kiro-provider-native-integration.md");
    assert!(
        !design.contains("encrypted reasoning replay are internal provider lifecycle concerns"),
        "the design doc still calls encrypted reasoning replay gateway-only, which is the assumption that left every request unsealed"
    );
    contains_all(
        "docs/design/kiro-provider-native-integration.md",
        &[
            "reasoningReplay",
            "reasoning.encrypted_content",
            "reasoning_replay_context_mismatch",
            "invalid_reasoning_replay",
        ],
    );

    // The retracted claim. `off` sends no sealed item and no `include`, but the
    // Responses input of every provider is now ordered as the model streamed it, so
    // no guide may promise a request byte-identical to earlier releases.
    for page in [
        "docs/reference/providers.md",
        "docs/reference/configuration.md",
        "docs/harness-runtime.md",
        "docs/zh/config/providers.md",
        "docs/zh/operate/harness-runtime.md",
    ] {
        let text = read(page);
        for claim in [
            "byte-identical to earlier",
            "sends exactly what earlier releases sent",
            "request bytes are unchanged",
            "与既有版本逐字节一致",
            "请求字节保持不变",
        ] {
            assert!(
                !text.contains(claim),
                "{page} still promises unchanged request bytes for an `off` provider: {claim}"
            );
        }
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
            "does not move the goal's revision",
            "only when the charge changes the goal's\nstatus",
            "### The host's default allowance",
            "8,000,000 tokens",
            "`TurnAllowance::UNLIMITED`",
            "told to set one",
            "does not carry over",
            "Under the default the turn\ncontinues",
            "no provider can withhold",
            "whether or not a Goal is active",
            "no durable counter to",
            "Nor is a session whose goal has finished",
            "does not stop a turn",
            "pause a goal the",
            "still charged to the goal",
            "`budget_limited` is the exception",
            "### Capability claims",
            "`capability_claim`",
            "`inferred`",
            "reports its previous state",
            "bedrock-model-capability-review",
            "### Generated state stays out of the commit",
            "refused before it runs",
            "git restore --staged",
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
            "并不会推进 Goal 的 revision",
            "只有当这次记账改变了 Goal 的状态时 revision 才会推进",
            "### 宿主的默认额度",
            "8,000,000",
            "`TurnAllowance::UNLIMITED`",
            "被告知去设一个",
            "有一条规则不适用于默认值",
            "但在默认额度下回合\n会继续",
            "provider 无法扣下的边界",
            "没有 Goal 的回合一样可以空转",
            "没有可供记账的持久计数器",
            "Goal 已经结束的会话同样不受影响",
            "不会让回合停止",
            "还会把模型已经完成的 Goal 改回暂停",
            "响应仍然会记账到这个 Goal 上",
            "`budget_limited` 是例外",
            "### 能力声明",
            "`capability_claim`",
            "`inferred`",
            "退回到更弱的状态",
            "bedrock-model-capability-review",
            "### 生成物不会进入提交",
            "拒绝信息点名这些路径",
            "git restore --staged",
        ],
    );
}

/// The navigation gate is off by default, so a user who never reads this page never
/// meets it. That makes the page the only place the three modes and the `.codegraph`
/// precondition are written down for a human, and a page that named the key without
/// its precondition would send someone to turn `strict` on in a repository with no
/// index and conclude the gate does nothing.
#[test]
fn configuration_reference_documents_the_navigation_gate() {
    contains_all(
        "docs/reference/configuration.md",
        &[
            "## Source navigation and the CodeGraph index",
            "`navigation.codegraph`",
            "`off` (default)",
            "`advise`",
            "`strict`",
            "inert unless the worktree root carries a `.codegraph` directory",
            "including `codegraph status`",
            "satisfy the gate nor violate it",
            "tracked per session",
        ],
    );
    contains_all(
        "docs/zh/config/reference.md",
        &[
            "## 源码导航与 CodeGraph 索引",
            "`navigation.codegraph`",
            "`off`（默认）",
            "`advise`",
            "`strict`",
            "只有 worktree 根目录存在 `.codegraph` 目录时这道门才生效",
            "`codegraph status`；`init`、`sync` 之类的索引生命周期子命令",
            "既不满足也不违反它",
            "这道门按会话跟踪",
        ],
    );
}

/// What the merge of the hardening work with #90 had to reconcile in the guides: a
/// spent allowance pauses rather than retries, a Goal's finished Plan is archived rather
/// than left to block the next Goal, and a client can tell a Zuno notice from a thought.
#[test]
fn release_docs_pin_allowance_pauses_retired_plans_and_tagged_notices() {
    contains_all(
        "docs/guide/sessions.md",
        &[
            "Agent step-limit, and eligible",
            "pauses the Goal with `turn_budget` and waits for a",
            "ends as `budget_limited` instead",
        ],
    );
    contains_all(
        "docs/zh/guide/sessions.md",
        &[
            "Agent 步数上限以及符合条件的工具失败",
            "以 `turn_budget` 暂停 Goal 并等待人工",
            "Goal 则进入 `budget_limited`",
        ],
    );
    contains_all(
        "AGENTS.md",
        &[
            "Agent step-limit, and eligible tool failures",
            "pauses with `turn_budget` and is never retried mechanically",
        ],
    );
    contains_all(
        "docs/harness-runtime.md",
        &[
            "### Turn allowances",
            "to 8,000,000 tokens",
            "`TurnAllowance::UNLIMITED`",
            "turn with `tool_call_budget` or `time_budget`",
            "stops the turn with `usage_unknown`; under the",
            "### Instruction file admission",
            "does not fit the instruction budget",
            "No notice is emitted for that case",
            "the `warning` notice `instruction.not_in_force` naming the source",
            "so the completion audit never judges the",
        ],
    );
    contains_all(
        "docs/zh/operate/harness-runtime.md",
        &[
            "Agent 步数上限和符合条件的工具失败",
            "以 `turn_budget` 暂停",
            "8,000,000 token 的宿主默认额度",
            "以 `tool_call_budget` 或 `time_budget` 停止",
            "用量不可测时以 `usage_unknown` 停止；宿主默认额度则继续执行",
            "code 为 `budget.<kind>`",
            "64 KB 与模型 context window 四分之一取较小值",
            "这种\n情况不会发出任何 notice",
            "`instruction.not_in_force` 报告哪个来源的规则本轮不生效",
            "归档为已完成的历史，完成审计不会再拿它对账新 Goal",
        ],
    );
    for (relative, needle) in [
        (
            "docs/guide/durable-state.md",
            "archived as completed history",
        ),
        ("docs/guide/tui.md", "archived as completed history"),
        (
            "docs/reference/zed-acp.md",
            "completed history and the panel is cleared",
        ),
        ("docs/zh/guide/durable-state.md", "归档为已完成的历史"),
        ("docs/zh/guide/tui.md", "归档为已完成的历史"),
        (
            "docs/zh/guide/editors.md",
            "归档为已完成的历史，面板随之清空",
        ),
    ] {
        contains_all(relative, &[needle]);
    }
    contains_all(
        "docs/reference/zed-acp.md",
        &[
            "`_meta.zuno.planId`, `revision`, `title`, and `stackDepth`",
            "`parentPlanId`",
            "`_meta.zuno.stepId`",
            "`_meta.zuno.cleared: true`",
            "`_meta.zuno.notice`",
            "a remote rule file that could not be fetched",
            "`instruction.*` or `budget.*` families",
        ],
    );
    contains_all(
        "docs/zh/cli/acp.md",
        &[
            "`_meta.zuno.planId`",
            "`stackDepth`",
            "`parentPlanId`",
            "`_meta.zuno.stepId`",
            "`_meta.zuno.cleared: true`",
            "`_meta.zuno.notice`",
            "无法抓取的远程规则文件",
            "`instruction.not_in_force`、`budget.compact`、`budget.token_budget`",
        ],
    );
    contains_all(
        "docs/cli/acp.md",
        &[
            "`_meta.zuno.notice`",
            "a remote rule file that could not be fetched",
            "`instruction.not_in_force`, `budget.compact`, or `budget.token_budget`",
        ],
    );
    contains_all(
        "docs/guide/headless.md",
        &[
            "{\"type\":\"notice\",\"severity\":\"warning\",\"code\":\"budget.token_budget\"",
            "while the turn still runs",
            "produces no `notice` event",
            "published on the server event stream as",
        ],
    );
    contains_all(
        "docs/zh/guide/headless.md",
        &[
            "{\"type\":\"notice\",\"severity\":\"warning\",\"code\":\"budget.token_budget\"",
            "但回合继续执行",
            "不会产生 `notice` 事件",
            "同一事件在 server 事件流上以 `notice` 发布",
        ],
    );
    contains_all(
        "docs/design/zed-acp-integration.md",
        &[
            "with one tagged exception",
            "`_meta.zuno.notice`",
            "`_meta.zuno.{planId,revision,title,stackDepth}`",
        ],
    );
    contains_all(
        "docs/zh/guide/editors.md",
        &[
            "唯一带标记的例外",
            "无法抓取的远程规则文件",
            "`_meta.zuno.notice`",
        ],
    );
    contains_all(
        "docs/guide/tui.md",
        &[
            "appear as\ntoasts whose level follows the notice severity",
            "They are not model output.",
        ],
    );
    contains_all(
        "docs/zh/guide/tui.md",
        &[
            "以 toast 显示，级别跟随通知的 severity",
            "它们不是模型输出。",
        ],
    );
    for (relative, retired) in [
        ("docs/guide/sessions.md", "turn-budget"),
        ("AGENTS.md", "turn-budget"),
        ("docs/zh/guide/sessions.md", "回合预算以及符合"),
        ("docs/zh/operate/harness-runtime.md", "回合预算和符合"),
        ("docs/reference/zed-acp.md", "historical Plan attached"),
        ("docs/harness-runtime.md", "silently truncated"),
        (
            "docs/harness-runtime.md",
            "with the typed notice `instruction.not_in_force`",
        ),
        ("docs/guide/headless.md", "which fails the turn"),
        ("docs/zh/guide/headless.md", "这会让本轮失败"),
        ("docs/cli/acp.md", "could not be admitted"),
        ("docs/zh/cli/acp.md", "无法进入 Prompt 的规则文件"),
        ("docs/reference/zed-acp.md", "could not be admitted"),
        ("docs/zh/guide/editors.md", "无法进入 Prompt 的规则文件"),
    ] {
        assert!(
            !read(relative).contains(retired),
            "{relative} still carries retired wording {retired:?}"
        );
    }
}

/// The two configuration index pages enumerate every top-level key by hand, and the count
/// sentence above the table is typed by hand too. A key added to the schema without a row
/// here is a feature the reference can be read cover to cover without discovering.
#[test]
fn config_index_tables_enumerate_every_schema_root_key() {
    let schema: serde_json::Value =
        serde_json::from_str(&read("schemas/zuno.json")).expect("schemas/zuno.json is JSON");
    let mut schema_keys: Vec<String> = schema["properties"]
        .as_object()
        .expect("schema root has properties")
        .keys()
        .cloned()
        .collect();
    schema_keys.push("$schema".to_owned());
    schema_keys.sort();
    schema_keys.dedup();
    let number_words = [
        (40, "Forty", "四十"),
        (41, "Forty-one", "四十一"),
        (42, "Forty-two", "四十二"),
        (43, "Forty-three", "四十三"),
        (44, "Forty-four", "四十四"),
        (45, "Forty-five", "四十五"),
        (46, "Forty-six", "四十六"),
        (47, "Forty-seven", "四十七"),
        (48, "Forty-eight", "四十八"),
    ];
    let (_, english, chinese) = number_words
        .iter()
        .find(|(count, _, _)| *count == schema_keys.len())
        .copied()
        .unwrap_or_else(|| panic!("extend number_words for {} keys", schema_keys.len()));

    for (relative, heading, count_sentence) in [
        (
            "docs/config/index.md",
            "## The top-level shape",
            format!("{english} keys exist."),
        ),
        (
            "docs/zh/config/index.md",
            "## 顶层结构",
            format!("一共有{chinese}个键。"),
        ),
    ] {
        let text = read(relative);
        let start = text
            .find(heading)
            .unwrap_or_else(|| panic!("{relative} has {heading}"));
        let section = &text[start..];
        let section = section[heading.len()..]
            .find("\n## ")
            .map_or(section, |end| &section[..heading.len() + end]);
        assert!(
            section.contains(&count_sentence),
            "{relative} must state {count_sentence:?} for {} schema keys",
            schema_keys.len()
        );
        let mut listed: Vec<String> = section
            .lines()
            .filter(|line| line.starts_with("| ") && line.contains('`'))
            .flat_map(|line| {
                line.split('`')
                    .skip(1)
                    .step_by(2)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect();
        listed.sort();
        listed.dedup();
        assert_eq!(
            listed, schema_keys,
            "{relative} top-level table drifted from schemas/zuno.json root properties"
        );
    }
}

/// The receipt's authority is the one shell fact a caller acts on, and the text-level
/// downgrade is invisible from the configuration table above it. A guide that stops at
/// `exitPolicy` teaches a reader to expect `authoritative` and read `derived` as a bug.
#[test]
fn tool_guides_explain_what_the_command_s_own_text_takes_back() {
    contains_all(
        "docs/guide/tools.md",
        &[
            "can take the guarantee",
            "written as an `if` or `while` condition exits zero when that check fails",
            "a `&&` chain before the last",
            "`$LASTEXITCODE` holding the second one's code alone",
            "What changes is the claim: it drops to `derived`",
            "nothing and keeps the configuration's verdict",
        ],
    );
    contains_all(
        "docs/zh/guide/tools.md",
        &[
            "因为跑在一个权威 shell 里的文本同样可以",
            "循环只报告最后一次迭代",
            "`$LASTEXITCODE` 里只剩后者的退出码",
            "它降为 `derived`",
            "什么都没掩盖",
        ],
    );
}

/// A built-in Skill count is the one fact on these pages that a new Skill falsifies,
/// and three pages carry it. Pinning the count together with the newest name keeps a
/// reader from trusting a list that silently stopped being complete.
#[test]
fn skill_pages_count_every_built_in_skill() {
    contains_all(
        "docs/guide/skills.md",
        &[
            "compiles eleven first-party Skills",
            "`bedrock-model-capability-review`",
        ],
    );
    contains_all(
        "docs/zh/guide/skills.md",
        &["十一个第一方 Skill", "`bedrock-model-capability-review`"],
    );
    contains_all(
        "docs/reference/configuration.md",
        &[
            "compiles eleven original first-party Skills",
            "`bedrock-model-capability-review`",
        ],
    );
}

#[test]
fn tui_docs_pin_the_input_discarded_when_the_terminal_is_released() {
    contains_all(
        "docs/guide/tui.md",
        &[
            "then discards the input it never read",
            "as a stray `0;54;31M` report",
        ],
    );
    contains_all(
        "docs/zh/guide/tui.md",
        &["再丢弃尚未读取的输入", "`0;54;31M` 这类残留报文"],
    );
}

/// A cancellation's certainty is the fact this batch found easiest to re-document wrongly.
///
/// Both pages once said a cooperative return was certain and that an uncertain cancellation
/// reports no exit code, and both claims were false — the second one for a guard `exit 125`
/// that had already exited and still decides nothing. These needles pin the corrected
/// contract in both languages, including that the demand reaches the model's own text.
#[test]
fn cancellation_docs_pin_certainty_as_a_verdict_and_not_a_mode() {
    contains_all(
        "docs/harness-runtime.md",
        &[
            "A cooperative settlement is not",
            "demand to the settled report the model reads",
            "keeps the certain cooperative reading, text included.",
            "not merely whether the process had exited",
            "Every other cancelled run preserves its captured output",
            "uncertain cancellation may still name an exit code",
        ],
    );
    contains_all(
        "docs/zh/operate/harness-runtime.md",
        &[
            "### 工具取消的确定性",
            "协作式取消并不自动等于结果确定。",
            "检查权威状态的那句话追加到模型读到的报告里",
            "区分它们的是服务是否把某个状态结算为命令自己的判定",
            "不确定的取消也可能给出退出码",
            "两种读法都绝不会被机械重放。",
        ],
    );
    contains_all(
        "docs/guide/tools.md",
        &["says nothing about whether the command"],
    );
    contains_all("docs/zh/guide/tools.md", &["对命令是否运行过一概不说"]);
}

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_final_assistant_message_sse_response;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::WorkflowDocumentFormat;
use codex_app_server_protocol::WorkflowListParams;
use codex_app_server_protocol::WorkflowListResponse;
use codex_app_server_protocol::WorkflowReadParams;
use codex_app_server_protocol::WorkflowReadResponse;
use codex_app_server_protocol::WorkflowRunCancelParams;
use codex_app_server_protocol::WorkflowRunCancelResponse;
use codex_app_server_protocol::WorkflowRunReadParams;
use codex_app_server_protocol::WorkflowRunReadResponse;
use codex_app_server_protocol::WorkflowRunStatus;
use codex_app_server_protocol::WorkflowRunUpdatedNotification;
use codex_app_server_protocol::WorkflowStartParams;
use codex_app_server_protocol::WorkflowStartResponse;
use codex_app_server_protocol::WorkflowValidateParams;
use codex_app_server_protocol::WorkflowValidateResponse;
use core_test_support::responses;
use serde_json::json;
#[cfg(unix)]
use std::io::Write;
use tempfile::TempDir;

const USER_GRAPH: &str = r#"# this comment must survive workflow/read
apiVersion: zuno.workflow/v1
kind: Workflow
metadata:
  name: review
  version: "1"
  description: user-owned review
spec:
  engine: graph/v1
  routes:
    work: { agentRef: native-codex }
  nodes:
    - { id: work, route: work }
"#;

#[tokio::test]
async fn workflow_control_plane_discovers_reads_validates_and_persists_runs() -> Result<()> {
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        create_final_assistant_message_sse_response("Done")?,
    )
    .await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;
    let workflows = codex_home.path().join("workflows");
    std::fs::create_dir_all(&workflows)?;
    std::fs::write(workflows.join("review.yaml"), USER_GRAPH)?;

    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;

    let parent_request = app_server
        .send_thread_start_request_with_auto_env(ThreadStartParams::default())
        .await?;
    let ThreadStartResponse { thread: parent, .. } = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        app_server.read_response(parent_request),
    )
    .await??;

    let list: WorkflowListResponse = app_server
        .request(|request_id| ClientRequest::WorkflowList {
            request_id,
            params: WorkflowListParams::default(),
        })
        .await?;
    assert!(list.diagnostics.is_empty(), "{:?}", list.diagnostics);
    assert_eq!(list.data.len(), 1);
    let summary = &list.data[0];
    assert_eq!(summary.identity.name, "review");
    assert_eq!(summary.description.as_deref(), Some("user-owned review"));
    assert_ne!(summary.workflow_id, "frontend-consensus");

    let outside = TempDir::new()?;
    let outside_cwd =
        codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(outside.path())?;
    let outside_request_id = app_server
        .send_request(
            "workflow/list",
            Some(json!({"cwds": [outside_cwd], "forceReload": true})),
        )
        .await?;
    let outside_error = app_server
        .read_stream_until_error_message(RequestId::Integer(outside_request_id))
        .await?;
    assert!(
        outside_error
            .error
            .message
            .contains("trusted working directory"),
        "{outside_error:?}"
    );

    let read: WorkflowReadResponse = app_server
        .request(|request_id| ClientRequest::WorkflowRead {
            request_id,
            params: WorkflowReadParams {
                workflow_id: summary.workflow_id.clone(),
            },
        })
        .await?;
    assert_eq!(read.document, USER_GRAPH);
    assert_eq!(read.definition["metadata"]["name"], json!("review"));

    std::fs::write(workflows.join("review.yaml"), vec![b'x'; 1024 * 1024 + 1])?;
    let oversized_request_id = app_server
        .send_request(
            "workflow/read",
            Some(json!({"workflowId": summary.workflow_id})),
        )
        .await?;
    let oversized_read = app_server
        .read_stream_until_error_message(RequestId::Integer(oversized_request_id))
        .await?;
    assert!(
        oversized_read
            .error
            .message
            .contains("exceeds the 1048576-byte read limit"),
        "{oversized_read:?}"
    );

    let invalid: WorkflowValidateResponse = app_server
        .request(|request_id| ClientRequest::WorkflowValidate {
            request_id,
            params: WorkflowValidateParams {
                source_id: "editor://draft".to_string(),
                format: WorkflowDocumentFormat::Yaml,
                document: "apiVersion: wrong/v1".to_string(),
            },
        })
        .await?;
    assert!(!invalid.valid);
    assert_eq!(invalid.diagnostics.len(), 1);

    let start_params = WorkflowStartParams {
        run_id: "run-review-1".to_string(),
        workflow_id: summary.workflow_id.clone(),
        expected_digest: summary.executable_digest.clone(),
        parent_thread_id: parent.id.clone(),
        args: json!({"issue": 42}),
    };
    let accepted: WorkflowStartResponse = app_server
        .request(|request_id| ClientRequest::WorkflowStart {
            request_id,
            params: start_params.clone(),
        })
        .await?;
    assert_eq!(accepted.run.run_id, "run-review-1");
    assert_eq!(accepted.run.status, WorkflowRunStatus::Queued);
    let admitted_update: WorkflowRunUpdatedNotification =
        app_server.read_notification("workflow/run/updated").await?;
    assert_eq!(admitted_update.run, accepted.run);

    let immediate_replay: WorkflowStartResponse = app_server
        .request(|request_id| ClientRequest::WorkflowStart {
            request_id,
            params: start_params.clone(),
        })
        .await?;
    assert_eq!(immediate_replay.run.run_id, accepted.run.run_id);
    assert_eq!(
        immediate_replay.run.request_digest,
        accepted.run.request_digest
    );

    let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let read: WorkflowRunReadResponse = app_server
                .request(|request_id| ClientRequest::WorkflowRunRead {
                    request_id,
                    params: WorkflowRunReadParams {
                        run_id: "run-review-1".to_string(),
                    },
                })
                .await?;
            if matches!(
                read.run.status,
                WorkflowRunStatus::Completed
                    | WorkflowRunStatus::Failed
                    | WorkflowRunStatus::Cancelled
                    | WorkflowRunStatus::Uncertain
            ) {
                break Ok::<_, anyhow::Error>(read.run);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await??;
    assert_eq!(
        terminal.status,
        WorkflowRunStatus::Completed,
        "{terminal:?}"
    );
    assert_eq!(terminal.binding_digest.len(), 64);
    assert_eq!(
        terminal.engine,
        codex_app_server_protocol::WorkflowEngine::GraphV1
    );
    assert_eq!(terminal.engine_revision, "graph/v1+zuno/v1");
    assert_eq!(
        terminal
            .result
            .as_ref()
            .and_then(|value| value["answer"].as_str()),
        Some("Done")
    );
    assert_eq!(terminal.agents_started, 1);

    let replay: WorkflowStartResponse = app_server
        .request(|request_id| ClientRequest::WorkflowStart {
            request_id,
            params: start_params.clone(),
        })
        .await?;
    assert_eq!(replay.run, terminal);

    let mut divergent = start_params;
    divergent.args = json!({"issue": 99});
    let divergent_request_id = app_server
        .send_request("workflow/start", Some(serde_json::to_value(divergent)?))
        .await?;
    let replay_error = app_server
        .read_stream_until_error_message(RequestId::Integer(divergent_request_id))
        .await?;
    assert!(
        replay_error
            .error
            .message
            .contains("already bound to a different start request"),
        "{replay_error:?}"
    );

    let cancelled_terminal: WorkflowRunCancelResponse = app_server
        .request(|request_id| ClientRequest::WorkflowRunCancel {
            request_id,
            params: WorkflowRunCancelParams {
                run_id: "run-review-1".to_string(),
                reason: Some("late duplicate cancellation".to_string()),
            },
        })
        .await?;
    assert_eq!(cancelled_terminal.run, terminal);

    let request = response_mock.single_request();
    assert!(
        request
            .message_input_texts("user")
            .iter()
            .any(|text| text.contains("Workflow context (JSON)")),
        "native Codex child must receive the generic graph node instruction"
    );

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn workflow_claude_backend_uses_user_execution_profile() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let codex_home = TempDir::new()?;
    let fixture_bin = codex_home.path().join("bin");
    std::fs::create_dir_all(&fixture_bin)?;
    let executable = fixture_bin.join("claude");
    std::fs::write(
        &executable,
        r#"#!/bin/sh
args=$(printf '%s ' "$@")
cat >/dev/null
printf '{"type":"result","subtype":"success","is_error":false,"result":"%s","session_id":"claude-fixture-session"}' "$args"
"#,
    )?;
    let mut permissions = std::fs::metadata(&executable)?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&executable, permissions)?;

    MockResponsesConfig::new("http://127.0.0.1:9").write(codex_home.path())?;
    std::fs::write(
        codex_home.path().join("claude-review.config.toml"),
        r#"model = "fable"
model_reasoning_effort = "high"
approval_policy = "never"
sandbox_mode = "read-only"
"#,
    )?;
    let workflows = codex_home.path().join("workflows");
    std::fs::create_dir_all(&workflows)?;
    std::fs::write(
        workflows.join("claude.yaml"),
        r#"apiVersion: zuno.workflow/v1
kind: Workflow
metadata:
  name: claude-review
  version: "1"
spec:
  engine: graph/v1
  routes:
    review: { agentRef: claude-code, executionProfile: claude-review }
  nodes:
    - id: review
      route: review
      input: Review the supplied change.
"#,
    )?;

    let inherited_path = std::env::var("PATH").unwrap_or_default();
    let child_path = format!("{}:{inherited_path}", fixture_bin.display());
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[("PATH", Some(child_path.as_str()))])
        .build_initialized()
        .await?;
    let parent_request = app_server
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            history_mode: Some(ThreadHistoryMode::Legacy),
            ..ThreadStartParams::default()
        })
        .await?;
    let ThreadStartResponse { thread: parent, .. } =
        app_server.read_response(parent_request).await?;
    let list: WorkflowListResponse = app_server
        .request(|request_id| ClientRequest::WorkflowList {
            request_id,
            params: WorkflowListParams::default(),
        })
        .await?;
    let workflow = list
        .data
        .iter()
        .find(|workflow| workflow.identity.name == "claude-review")
        .expect("user Claude workflow");
    let run_id = "run-claude-profile".to_string();
    let _accepted: WorkflowStartResponse = app_server
        .request(|request_id| ClientRequest::WorkflowStart {
            request_id,
            params: WorkflowStartParams {
                run_id: run_id.clone(),
                workflow_id: workflow.workflow_id.clone(),
                expected_digest: workflow.executable_digest.clone(),
                parent_thread_id: parent.id.clone(),
                args: json!({"change": "typed backend routing"}),
            },
        })
        .await?;

    let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let read: WorkflowRunReadResponse = app_server
                .request(|request_id| ClientRequest::WorkflowRunRead {
                    request_id,
                    params: WorkflowRunReadParams {
                        run_id: run_id.clone(),
                    },
                })
                .await?;
            if matches!(
                read.run.status,
                WorkflowRunStatus::Completed
                    | WorkflowRunStatus::Failed
                    | WorkflowRunStatus::Cancelled
                    | WorkflowRunStatus::Uncertain
            ) {
                break Ok::<_, anyhow::Error>(read.run);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await??;

    assert_eq!(
        terminal.status,
        WorkflowRunStatus::Completed,
        "{terminal:?}"
    );
    assert_eq!(
        terminal
            .result
            .as_ref()
            .and_then(|value| value["answer"].as_str()),
        Some(
            "--print --output-format json --no-session-persistence --permission-mode plan --disallowedTools AskUserQuestion,ExitPlanMode --model fable --effort high "
        )
    );
    assert_eq!(
        terminal
            .result
            .as_ref()
            .and_then(|value| value["backend"].as_str()),
        Some("claude-code")
    );
    assert_eq!(
        terminal
            .result
            .as_ref()
            .and_then(|value| value["sessionId"].as_str()),
        Some("claude-fixture-session")
    );
    std::fs::write(
        codex_home.path().join("claude-full.config.toml"),
        r#"model = "fable-full"
model_reasoning_effort = "max"
approval_policy = "never"
default_permissions = ":danger-full-access"
"#,
    )?;
    std::fs::write(
        workflows.join("claude-full.yaml"),
        r#"apiVersion: zuno.workflow/v1
kind: Workflow
metadata:
  name: claude-full
  version: "1"
spec:
  engine: graph/v1
  routes:
    build: { agentRef: claude-code, executionProfile: claude-full }
  nodes:
    - id: build
      route: build
      input: Implement the approved change.
"#,
    )?;
    let refreshed: WorkflowListResponse = app_server
        .request(|request_id| ClientRequest::WorkflowList {
            request_id,
            params: WorkflowListParams {
                force_reload: true,
                ..WorkflowListParams::default()
            },
        })
        .await?;
    let full_workflow = refreshed
        .data
        .iter()
        .find(|workflow| workflow.identity.name == "claude-full")
        .expect("full-access Claude workflow");
    let full_run_id = "run-claude-full".to_string();
    let _accepted: WorkflowStartResponse = app_server
        .request(|request_id| ClientRequest::WorkflowStart {
            request_id,
            params: WorkflowStartParams {
                run_id: full_run_id.clone(),
                workflow_id: full_workflow.workflow_id.clone(),
                expected_digest: full_workflow.executable_digest.clone(),
                parent_thread_id: parent.id.clone(),
                args: json!({"change": "explicit full access"}),
            },
        })
        .await?;
    let full_terminal = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let read: WorkflowRunReadResponse = app_server
                .request(|request_id| ClientRequest::WorkflowRunRead {
                    request_id,
                    params: WorkflowRunReadParams {
                        run_id: full_run_id.clone(),
                    },
                })
                .await?;
            if matches!(
                read.run.status,
                WorkflowRunStatus::Completed
                    | WorkflowRunStatus::Failed
                    | WorkflowRunStatus::Cancelled
                    | WorkflowRunStatus::Uncertain
            ) {
                break Ok::<_, anyhow::Error>(read.run);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await??;
    assert_eq!(
        full_terminal.status,
        WorkflowRunStatus::Completed,
        "{full_terminal:?}"
    );
    let full_answer = full_terminal
        .result
        .as_ref()
        .and_then(|value| value["answer"].as_str())
        .expect("full-access Claude answer");
    for expected in [
        "--model fable-full",
        "--effort max",
        "--permission-mode bypassPermissions",
        "--dangerously-skip-permissions",
    ] {
        assert!(
            full_answer.contains(expected),
            "missing {expected:?} in {full_answer:?}"
        );
    }

    std::fs::write(
        codex_home.path().join("claude-review.config.toml"),
        r#"model = "changed-after-admission"
model_reasoning_effort = "high"
approval_policy = "never"
sandbox_mode = "read-only"
"#,
    )?;
    let drift_request = WorkflowStartParams {
        run_id,
        workflow_id: workflow.workflow_id.clone(),
        expected_digest: workflow.executable_digest.clone(),
        parent_thread_id: parent.id,
        args: json!({"change": "typed backend routing"}),
    };
    let drift_id = app_server
        .send_request("workflow/start", Some(serde_json::to_value(drift_request)?))
        .await?;
    let drift = app_server
        .read_stream_until_error_message(RequestId::Integer(drift_id))
        .await?;
    assert!(
        drift.error.message.contains("different start request"),
        "{drift:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn workflow_mounts_plugin_declared_acp_backend_with_execution_profile() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new("http://127.0.0.1:9").write(codex_home.path())?;
    std::fs::OpenOptions::new()
        .append(true)
        .open(codex_home.path().join("config.toml"))?
        .write_all(
            br#"
[features]
plugins = true

[plugins."sample@test"]
enabled = true
"#,
        )?;
    std::fs::write(
        codex_home.path().join("plugin-acp.config.toml"),
        r#"model = "profile-model"
model_reasoning_effort = "high"
approval_policy = "never"
sandbox_mode = "read-only"
"#,
    )?;

    let plugin_root = codex_home.path().join("plugins/cache/test/sample/local");
    let plugin_manifest = plugin_root.join(".codex-plugin/plugin.json");
    let plugin_workflows = plugin_root.join("workflows");
    let plugin_bin = plugin_root.join("bin");
    std::fs::create_dir_all(plugin_manifest.parent().expect("manifest parent"))?;
    std::fs::create_dir_all(&plugin_workflows)?;
    std::fs::create_dir_all(&plugin_bin)?;
    std::fs::write(
        plugin_manifest,
        r#"{
  "name": "sample",
  "workflows": "./workflows",
  "agentBackends": "./agent-backends.json"
}"#,
    )?;
    std::fs::write(
        plugin_root.join("agent-backends.json"),
        r#"{
  "apiVersion": "zuno.agent-backends/v1",
  "backends": {
    "review": {
      "kind": "acp",
      "command": "./bin/fake-acp.sh",
      "args": ["--stdio"]
    }
  }
}"#,
    )?;
    std::fs::write(
        plugin_workflows.join("review.yaml"),
        r#"apiVersion: zuno.workflow/v1
kind: Workflow
metadata:
  name: plugin-acp-review
  version: "1"
spec:
  engine: graph/v1
  routes:
    review: { agentRef: sample/review, executionProfile: plugin-acp }
  nodes:
    - id: review
      route: review
      input: Review the supplied change.
"#,
    )?;

    let executable = plugin_bin.join("fake-acp.sh");
    std::fs::write(
        &executable,
        r#"#!/bin/sh
[ "$1" = "--stdio" ] || exit 91
seen=""
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      seen="$seen initialize"
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{},"authMethods":[]}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      case "$line" in *'"model":"profile-model"'*) seen="$seen session-model";; esac
      case "$line" in *'"modelProvider":"mock_provider"'*) seen="$seen session-provider";; esac
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"plugin-acp-session"}}\n' "$id"
      ;;
    *'"method":"session/set_model"'*)
      case "$line" in *'"modelId":"profile-model"'*) seen="$seen set-model";; esac
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
      ;;
    *'"configId":"reasoning_effort"'*)
      case "$line" in *'"value":"high"'*) seen="$seen effort";; esac
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
      ;;
    *'"configId":"permissions"'*)
      case "$line" in *'"value":":read-only"'*) seen="$seen permissions";; esac
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
      ;;
    *'"method":"session/prompt"'*)
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"plugin-acp-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"plugin ACP answer%s prompt"}}}}\n' "$seen"
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
  esac
done
"#,
    )?;
    let mut permissions = std::fs::metadata(&executable)?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&executable, permissions)?;

    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let parent_request = app_server
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            history_mode: Some(ThreadHistoryMode::Legacy),
            ..ThreadStartParams::default()
        })
        .await?;
    let ThreadStartResponse { thread: parent, .. } =
        app_server.read_response(parent_request).await?;
    let list: WorkflowListResponse = app_server
        .request(|request_id| ClientRequest::WorkflowList {
            request_id,
            params: WorkflowListParams::default(),
        })
        .await?;
    let workflow = list
        .data
        .iter()
        .find(|workflow| workflow.identity.name == "plugin-acp-review")
        .expect("plugin workflow");
    assert_eq!(
        workflow.scope,
        codex_app_server_protocol::WorkflowScope::Plugin
    );

    let run_id = "run-plugin-acp".to_string();
    let start_params = WorkflowStartParams {
        run_id: run_id.clone(),
        workflow_id: workflow.workflow_id.clone(),
        expected_digest: workflow.executable_digest.clone(),
        parent_thread_id: parent.id,
        args: json!({"change": "declaration-driven factory mounting"}),
    };
    let _accepted: WorkflowStartResponse = app_server
        .request(|request_id| ClientRequest::WorkflowStart {
            request_id,
            params: start_params.clone(),
        })
        .await?;

    let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let read: WorkflowRunReadResponse = app_server
                .request(|request_id| ClientRequest::WorkflowRunRead {
                    request_id,
                    params: WorkflowRunReadParams {
                        run_id: run_id.clone(),
                    },
                })
                .await?;
            if matches!(
                read.run.status,
                WorkflowRunStatus::Completed
                    | WorkflowRunStatus::Failed
                    | WorkflowRunStatus::Cancelled
                    | WorkflowRunStatus::Uncertain
            ) {
                break Ok::<_, anyhow::Error>(read.run);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await??;

    assert_eq!(
        terminal.status,
        WorkflowRunStatus::Completed,
        "{terminal:?}"
    );
    let result = terminal.result.as_ref().expect("workflow result");
    assert_eq!(
        result["answer"],
        json!(
            "plugin ACP answer initialize session-model session-provider set-model effort permissions prompt"
        )
    );
    assert_eq!(result["backend"], json!("acp"));
    assert_eq!(result["sessionId"], json!("plugin-acp-session"));
    std::fs::write(
        codex_home.path().join("plugin-acp.config.toml"),
        r#"model = "changed-after-admission"
model_reasoning_effort = "high"
approval_policy = "never"
sandbox_mode = "read-only"
"#,
    )?;
    let replay_id = app_server
        .send_request("workflow/start", Some(serde_json::to_value(start_params)?))
        .await?;
    let replay_error = app_server
        .read_stream_until_error_message(RequestId::Integer(replay_id))
        .await?;
    assert!(
        replay_error
            .error
            .message
            .contains("already bound to a different start request"),
        "{replay_error:?}"
    );
    Ok(())
}

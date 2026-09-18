//! Strict approval policy (`approval_policy = "untrusted"`): every terminal
//! input is reviewed by the user, even without the `write_stdin_approval`
//! feature and even when the terminal's permissions did not change.

use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_core::config::Constrained;
use codex_protocol::approvals::ExecApprovalKind;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_sandbox;
use core_test_support::skip_if_target_windows;
use core_test_support::test_codex::TestCodexHarness;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

fn tool_response(id: &str, tool: &str, args: Value) -> String {
    sse(vec![
        ev_function_call(id, tool, &args.to_string()),
        ev_completed(id),
    ])
}

/// A server-style strict profile: no sandbox, approval before everything.
async fn strict_harness() -> Result<TestCodexHarness> {
    TestCodexHarness::with_auto_env_builder(test_codex().with_config(|config| {
        #[allow(deprecated)]
        let cwd = config.cwd.to_path_buf();
        config
            .permissions
            .set_legacy_sandbox_policy(SandboxPolicy::DangerFullAccess, &cwd)
            .expect("set full access sandbox");
        config.permissions.approval_policy = Constrained::allow_any(AskForApproval::UnlessTrusted);
    }))
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strict_policy_reviews_every_terminal_input() -> Result<()> {
    skip_if_target_windows!(Ok(()), "uses a POSIX interactive shell");
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    let harness = strict_harness().await?;
    let test = harness.test();

    let calls = mount_sse_sequence(
        harness.server(),
        vec![
            tool_response(
                "open",
                "exec_command",
                json!({"cmd":"/bin/bash --noprofile --norc", "tty":true, "yield_time_ms":200}),
            ),
            tool_response(
                "denied",
                "write_stdin",
                json!({"session_id":1000, "chars":"printf denied > marker\n", "yield_time_ms":1000}),
            ),
            tool_response(
                "allowed",
                "write_stdin",
                json!({"session_id":1000, "chars":"printf allowed > marker; exit\n", "yield_time_ms":1000}),
            ),
            sse(vec![ev_completed("done")]),
        ],
    )
    .await;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "open a shell and write the marker".into(),
            text_elements: Vec::new(),
        }]))
        .await?;

    // Opening the shell is a command: strict mode asks first.
    let open = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ExecApprovalRequest(request) => Some(request.clone()),
        _ => None,
    })
    .await;
    assert_eq!(
        (open.kind, open.call_id.as_str()),
        (ExecApprovalKind::Command, "open")
    );
    test.codex
        .submit(Op::ExecApproval {
            id: "open".into(),
            turn_id: Some(open.turn_id),
            decision: ReviewDecision::Approved,
        })
        .await?;

    // Every later input to the approved shell is reviewed again, without the
    // write_stdin_approval feature and without any permission change.
    for (id, decision) in [
        ("denied", ReviewDecision::denied("not this one")),
        ("allowed", ReviewDecision::Approved),
    ] {
        let request = wait_for_event_match(&test.codex, |event| match event {
            EventMsg::ExecApprovalRequest(request) => Some(request.clone()),
            _ => None,
        })
        .await;
        assert_eq!(
            (
                request.kind,
                request.call_id.as_str(),
                request.effective_approval_id().as_str()
            ),
            (ExecApprovalKind::WriteStdin, "open", id)
        );
        assert!(
            request
                .reason
                .as_deref()
                .is_some_and(|reason| reason.starts_with("Strict approval policy:")),
            "{:?}",
            request.reason
        );
        test.codex
            .submit(Op::ExecApproval {
                id: id.into(),
                turn_id: Some(request.turn_id),
                decision,
            })
            .await?;
    }
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    // The denied input never reached the shell; only the approved one ran.
    assert_eq!(harness.read_file_text("marker").await?, "allowed");
    let requests = calls.requests();
    let denied_output = requests
        .last()
        .expect("final model request")
        .function_call_output("denied")
        .to_string();
    assert!(
        denied_output.contains("not this one"),
        "denied input should report the rejection: {denied_output}"
    );
    Ok(())
}

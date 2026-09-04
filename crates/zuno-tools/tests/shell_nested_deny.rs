//! A configured `shell` deny reaches the command a wrapper or an inline script runs.
//!
//! The permission layer matches one flattened command line, so `sh -c 'rm -rf /'` was
//! the program `sh` to a deny written as `rm -rf*` and answered Ask. `ShellTool` now
//! asks for the nested command lines as well, and the engine refuses as soon as any
//! pattern is denied. These tests drive the production tool with an asker that applies
//! a rule set the way `zuno_permission::PermissionEngine` and the engine's
//! `RulePermissionAsker` do: any denied pattern denies the call.
#![cfg(unix)]

mod support;

use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use zuno_error::ToolError;
use zuno_permission::{PermissionAction, Rule, evaluate};
use zuno_pty::BackgroundExecutionPurpose;
use zuno_tool::{NeverInterrupted, PermissionAsk, PermissionAsker, PermissionOrigin, ToolContext};
use zuno_tools::shell::ShellParams;

/// Applies one rule set to every pattern of every ask, recording what was asked.
struct RuleAsker {
    rules: Vec<Rule>,
    asks: Mutex<Vec<PermissionAsk>>,
}

impl RuleAsker {
    fn new(rules: &[(&str, PermissionAction)]) -> Arc<Self> {
        Arc::new(Self {
            rules: rules
                .iter()
                .map(|(pattern, action)| Rule {
                    permission: "shell".to_owned(),
                    pattern: (*pattern).to_owned(),
                    action: *action,
                })
                .collect(),
            asks: Mutex::new(Vec::new()),
        })
    }

    fn shell_asks(&self) -> Vec<PermissionAsk> {
        self.asks
            .lock()
            .expect("ask log")
            .iter()
            .filter(|ask| ask.permission == "shell")
            .cloned()
            .collect()
    }
}

#[async_trait]
impl PermissionAsker for RuleAsker {
    async fn ask(
        &self,
        _origin: PermissionOrigin<'_>,
        tool: &str,
        ask: PermissionAsk,
    ) -> Result<(), ToolError> {
        self.asks.lock().expect("ask log").push(ask.clone());
        if ask.permission != "shell" {
            return Ok(());
        }
        for pattern in &ask.patterns {
            if evaluate(&ask.permission, pattern, &self.rules) == PermissionAction::Deny {
                return Err(ToolError::Denied {
                    tool: tool.to_owned(),
                });
            }
        }
        Ok(())
    }
}

fn context(asker: Arc<RuleAsker>) -> ToolContext {
    ToolContext::new(
        "ses_nested",
        "msg_nested",
        "call_nested",
        "build",
        asker,
        Arc::new(NeverInterrupted),
    )
}

fn params(command: &str) -> ShellParams {
    ShellParams {
        command: command.to_owned(),
        timeout: None,
        workdir: None,
        background: false,
        background_purpose: BackgroundExecutionPurpose::Command,
        expected_git_head: None,
        exit_policy: None,
    }
}

const DENY_RM_RF: &[(&str, PermissionAction)] = &[
    ("*", PermissionAction::Ask),
    ("rm -rf*", PermissionAction::Deny),
];

/// Every wrapper form the audit listed, with a target the risk gate holds for
/// confirmation rather than refuses outright, so the call reaches the permission layer.
#[tokio::test]
async fn a_configured_deny_reaches_the_command_a_wrapper_or_inline_script_runs() {
    let workspace = tempfile::tempdir().expect("workspace");
    let tool = support::sandbox::shell_tool(workspace.path());
    for command in [
        "sh -c 'rm -rf ./build'",
        "bash -c \"rm -rf ./build\"",
        "zsh -c 'rm -rf ./build'",
        "dash -c 'rm -rf ./build'",
        "bash -lc 'rm -rf ./build'",
        "env FOO=1 rm -rf ./build",
        "env -S 'rm -rf ./build'",
        "command rm -rf ./build",
        "command -p rm -rf ./build",
        "nice -n 5 rm -rf ./build",
        "nohup rm -rf ./build",
        "timeout 5 rm -rf ./build",
        "sudo -u root rm -rf ./build",
        "su root -c 'rm -rf ./build'",
        "eval 'rm -rf ./build'",
        "env nice sh -c 'rm -rf ./build'",
        "sh -c 'echo hi; rm -rf ./build'",
        "sh -c \"sh -c 'rm -rf ./build'\"",
        "sudo sh -c 'rm -rf ./build'",
        "echo hi && sh -c 'rm -rf ./build'",
    ] {
        let asker = RuleAsker::new(DENY_RM_RF);
        let error = tool
            .run(params(command), context(Arc::clone(&asker)))
            .await
            .expect_err(&format!("{command:?} must be denied under `rm -rf*`"));
        assert!(
            matches!(error, ToolError::Denied { .. }),
            "{command:?} must be refused by the permission layer, got {error:?}"
        );
        let asks = asker.shell_asks();
        assert_eq!(asks.len(), 1, "{command:?} raised one shell ask: {asks:#?}");
        assert!(
            asks[0]
                .patterns
                .iter()
                .any(|pattern| pattern == "rm -rf ./build"),
            "{command:?} must ask for the command it really runs: {:?}",
            asks[0].patterns
        );
        assert!(
            !workspace.path().join("build").exists(),
            "nothing ran for {command:?}"
        );
    }
}

/// The audit's exact input. The unconfigurable catastrophic table refuses it before the
/// permission layer is consulted, so no ask is raised at all; the configured deny is the
/// second line, pinned above with a target the table does not own.
#[tokio::test]
async fn the_audits_exact_input_is_refused_before_any_ask() {
    let workspace = tempfile::tempdir().expect("workspace");
    let tool = support::sandbox::shell_tool(workspace.path());
    let asker = RuleAsker::new(DENY_RM_RF);
    let error = tool
        .run(params("sh -c 'rm -rf /'"), context(Arc::clone(&asker)))
        .await
        .expect_err("`sh -c 'rm -rf /'` never runs");
    assert!(
        matches!(&error, ToolError::Failed { source, .. } if source.to_string().contains("blocked")),
        "the catastrophic table refuses it first: {error:?}"
    );
    assert!(
        asker.shell_asks().is_empty(),
        "no permission ask was reached"
    );
}

/// The nested line is an extra pattern, not a replacement: the call still asks for the
/// line as written, and a rule that allows both still allows.
#[tokio::test]
async fn a_wrapper_whose_inner_command_is_not_denied_still_runs() {
    let workspace = tempfile::tempdir().expect("workspace");
    let tool = support::sandbox::shell_tool(workspace.path());

    let asker = RuleAsker::new(DENY_RM_RF);
    let output = tool
        .run(
            params("sh -c 'echo nested-ok'"),
            context(Arc::clone(&asker)),
        )
        .await
        .expect("a wrapper around an undenied command runs");
    assert!(output.output.contains("nested-ok"), "{}", output.output);
    let asks = asker.shell_asks();
    assert_eq!(asks.len(), 1);
    assert_eq!(
        asks[0].patterns,
        vec![
            "sh -c 'echo nested-ok'".to_owned(),
            "echo nested-ok".to_owned()
        ],
        "the line as written comes first, the line it runs second"
    );
    assert!(
        asks[0].always.iter().any(|pattern| pattern == "echo *"),
        "an always reply covers the inner shape too: {:?}",
        asks[0].always
    );

    let allow_all = RuleAsker::new(&[("*", PermissionAction::Allow)]);
    let output = tool
        .run(
            params("env FOO=1 sh -c 'echo still-ok'"),
            context(Arc::clone(&allow_all)),
        )
        .await
        .expect("an allow-all rule set still allows a wrapped command");
    assert!(output.output.contains("still-ok"), "{}", output.output);
}

/// A deny is never traded for an ask: the direct spelling stays denied, and a
/// pipeline stage fed by `xargs` is left to the risk gate rather than read as a command
/// line it is not.
#[tokio::test]
async fn the_direct_spelling_stays_denied_and_xargs_is_not_read_as_a_command_line() {
    let workspace = tempfile::tempdir().expect("workspace");
    let tool = support::sandbox::shell_tool(workspace.path());

    let asker = RuleAsker::new(DENY_RM_RF);
    let error = tool
        .run(params("rm -rf ./build"), context(Arc::clone(&asker)))
        .await
        .expect_err("the direct spelling is denied as before");
    assert!(matches!(error, ToolError::Denied { .. }), "{error:?}");

    let asker = RuleAsker::new(DENY_RM_RF);
    let _ = tool
        .run(
            params("printf ./build | xargs rm -rf"),
            context(Arc::clone(&asker)),
        )
        .await;
    let asks = asker.shell_asks();
    assert_eq!(asks.len(), 1, "{asks:#?}");
    assert!(
        !asks[0].patterns.iter().any(|pattern| pattern == "rm -rf"),
        "what follows xargs is a prefix fed by stdin, not a command line: {:?}",
        asks[0].patterns
    );
}

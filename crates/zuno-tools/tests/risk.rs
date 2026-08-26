use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use zuno_error::ToolError;
use zuno_tool::{NeverInterrupted, PermissionAsk, PermissionAsker, ToolContext};
use zuno_tools::risk::{GateOutcome, RiskContext, assess_command, gate};
use zuno_tools::shell::{ShellParams, ShellSyntax, ShellTool};

fn risk_context() -> RiskContext {
    RiskContext {
        working_dir: Some(PathBuf::from("/work/project")),
        home_dir: Some(PathBuf::from("/home/alice")),
    }
}

fn risk_gate(command: &str) -> GateOutcome {
    let assessment = assess_command(command, ShellSyntax::Bash, &risk_context())
        .expect("the command must parse");
    gate(&assessment)
}

fn assert_risk_denied(command: &str) {
    let outcome = risk_gate(command);
    assert!(
        matches!(outcome, GateOutcome::Deny { .. }),
        "expected permanent denial for {command:?}, got {outcome:?}"
    );
}

fn assert_risk_denied_with_syntax(command: &str, syntax: ShellSyntax) {
    let assessment =
        assess_command(command, syntax, &risk_context()).expect("the command must parse");
    let outcome = gate(&assessment);
    assert!(
        matches!(outcome, GateOutcome::Deny { .. }),
        "expected permanent denial for {command:?} under {syntax:?}, got {outcome:?} from {assessment:#?}"
    );
}

#[test]
fn risk_user_reported_catastrophic_probe_baseline_stays_denied() {
    for command in [
        "sh -c 'rm -rf /'",
        "eval 'rm -rf /'",
        r"find / -exec rm -rf {} \;",
        "xargs rm -rf /",
        "echo x; rm -rf /",
        "( rm -rf / )",
        r"rm -rf \/",
        "rm -rf //",
        "rm -rf /.",
        "rm -rf --no-preserve-root /",
        "rm -rf $HOME",
        "rm -rf ~/.ssh",
        "dd of=/dev/sda",
        "truncate -s0 /etc/passwd",
        "su -c 'rm -rf /'",
        "rm -rf ../../..",
    ] {
        assert_risk_denied(command);
    }
}

#[test]
fn risk_empty_brace_alternatives_cannot_hide_a_catastrophic_target() {
    for command in ["rm -rf /{,}", "rm -rf /{a,}", "rm -rf {/,}", "rm -rf /{.,}"] {
        assert_risk_denied(command);
    }
}

#[test]
fn risk_directory_changes_are_applied_before_relative_destructive_targets() {
    for command in [
        "cd / && rm -rf .",
        "cd / && rm -rf *",
        "cd ~ && rm -rf .",
        "cd / ; rm -rf .",
        "pushd / && rm -rf .",
        "cd /etc && rm -rf .",
        "cd / && rm -rf ./",
        "chdir / && rm -rf .",
        "pushd / && pushd /work/project && popd && rm -rf .",
    ] {
        assert_risk_denied(command);
    }

    for command in [
        "cd \"$TARGET\" && rm -rf .",
        "cd - && rm -rf .",
        "pushd \"$TARGET\" && rm -rf *",
    ] {
        assert_risk_denied(command);
    }

    for command in [
        "Set-Location /; Remove-Item -Recurse .",
        "Push-Location /; Remove-Item -Recurse .",
        "Push-Location /; Push-Location /work/project; Pop-Location; Remove-Item -Recurse .",
    ] {
        assert_risk_denied_with_syntax(command, ShellSyntax::PowerShell);
    }
}

#[test]
fn risk_denies_every_lexical_spelling_of_the_filesystem_root() {
    for target in ["/", "/.", "//", "/..", "///", "/tmp/../"] {
        assert_risk_denied(&format!("rm -rf --no-preserve-root -- {target}"));
    }
    assert_risk_denied("rm -rf ../../..");
}

#[test]
fn risk_denies_the_users_home_in_symbolic_and_literal_forms() {
    for target in ["$HOME", "${HOME}", "~", "/home/alice", "~/."] {
        assert_risk_denied(&format!("rm -rf {target}"));
    }
    assert!(matches!(
        risk_gate("rm -rf ~other"),
        GateOutcome::Confirm { .. }
    ));
}

#[test]
fn risk_denies_credential_stores_recursively() {
    for target in [
        "~/.ssh/id_ed25519",
        "~/.gnupg/private-keys-v1.d/key",
        "~/.aws/credentials",
        "~/.kube/config",
        "~/.docker/config.json",
        "~/.config/gh/hosts.yml",
        "~/.netrc",
        "~/.password-store/example.gpg",
        "~/.local/share/keyrings/login.keyring",
    ] {
        assert_risk_denied(&format!("rm -f {target}"));
    }
}

#[test]
fn risk_denies_device_nodes_but_allows_the_null_redirect_sink() {
    assert_risk_denied("printf x > /dev/sda");
    assert_risk_denied("rm -f /dev/null");
    assert_eq!(risk_gate("foo > /dev/null"), GateOutcome::Allow);
}

#[test]
fn risk_compound_command_cannot_hide_a_catastrophic_constituent() {
    assert_risk_denied("echo hi && rm -rf /");
    assert_risk_denied("(echo hi; rm -rf /)");
}

#[test]
fn risk_wrappers_and_shell_spelling_cannot_hide_a_catastrophic_command() {
    for command in [
        r"r\m -rf /",
        "r'm' -rf /",
        "'r''m' -rf /",
        "sudo -u root rm -rf /",
        "env SAFE=1 timeout 1s rm -rf /",
        "timeout 1d rm -rf /",
        "chroot /mnt /bin/rm -rf /",
        "bash -lc 'rm -rf /'",
        "su root -c 'rm -rf /'",
        "sudo su root -c 'rm -rf /'",
        "eval 'rm -rf /'",
    ] {
        assert_risk_denied(command);
    }
}

#[test]
fn risk_find_exec_and_xargs_cannot_hide_a_catastrophic_target() {
    assert_risk_denied("find /tmp -exec rm -rf / {} +");
    assert_risk_denied("find -- / -delete");
    assert_risk_denied("printf x | xargs rm -rf /");
}

#[test]
fn risk_static_brace_expansion_checks_every_target() {
    assert_risk_denied("rm -rf /{etc,var}");
    assert_risk_denied("rm -rf /e''tc");
    assert_risk_denied("printf x > /e''tc/passwd");
    assert!(matches!(
        risk_gate("rm -rf /tmp/{1..3}"),
        GateOutcome::Confirm { .. }
    ));
}

#[test]
fn risk_bounded_destructive_command_requires_human_confirmation() {
    let first = risk_gate("rm -rf ./build");
    let GateOutcome::Confirm { reason, target } = first else {
        panic!("a bounded destructive command must require confirmation");
    };
    assert!(
        reason.contains("irreversibly removes or overwrites data"),
        "{reason}"
    );
    assert_eq!(target.as_deref(), Some("/work/project/build"));
}

#[test]
fn risk_unknown_dynamic_delete_target_requires_confirmation_instead_of_guessing() {
    for command in [
        "rm -rf \"$UNSET_VAR/\"",
        "find \"$TARGET\" -delete",
        "printf '%s' target | xargs rm -rf",
    ] {
        assert!(
            matches!(risk_gate(command), GateOutcome::Confirm { .. }),
            "unknown runtime target must be held: {command:?}"
        );
    }
    let dynamic_command = risk_gate("$DELETE_COMMAND -rf /");
    assert!(
        matches!(dynamic_command, GateOutcome::Confirm { .. }),
        "dynamic command names must require confirmation, got {dynamic_command:?}"
    );
}

#[test]
fn risk_benign_command_is_never_gated() {
    for command in [
        "ls -la",
        "git status --short",
        "foo > /dev/null",
        "cat ~/.ssh/id_rsa",
    ] {
        assert_eq!(risk_gate(command), GateOutcome::Allow);
    }
}

#[test]
fn risk_new_file_in_the_system_temp_directory_is_not_an_overwrite() {
    let workspace = tempfile::tempdir().expect("workspace");
    let scratch = tempfile::tempdir().expect("scratch");
    let target = scratch.path().join("fresh-puzzle.py");
    let context = RiskContext {
        working_dir: Some(workspace.path().to_owned()),
        home_dir: None,
    };
    let assessment = assess_command(
        &format!("printf puzzle > '{}'", target.display()),
        ShellSyntax::Bash,
        &context,
    )
    .expect("the command must parse");

    assert_eq!(
        gate(&assessment),
        GateOutcome::Allow,
        "a path that does not exist in the OS temp directory is creation, not truncation"
    );
}

#[test]
fn risk_absent_forced_temp_file_cleanup_is_not_treated_as_data_loss() {
    let workspace = tempfile::tempdir().expect("workspace");
    let scratch = tempfile::tempdir().expect("scratch");
    let target = scratch.path().join("zuno-strip-probe");
    let context = RiskContext {
        working_dir: Some(workspace.path().to_owned()),
        home_dir: None,
    };
    let command = format!(
        "cp target/release/zuno '{}' && strip '{}' && rm -f '{}'",
        target.display(),
        target.display(),
        target.display()
    );
    let assessment =
        assess_command(&command, ShellSyntax::Bash, &context).expect("the command must parse");

    assert_eq!(
        gate(&assessment),
        GateOutcome::Allow,
        "removing an exact, currently absent non-directory temp path is cleanup, not data loss"
    );

    std::fs::write(&target, b"keep").expect("existing target");
    let existing = assess_command(
        &format!("rm -f '{}'", target.display()),
        ShellSyntax::Bash,
        &context,
    )
    .expect("the command must parse");
    assert!(
        matches!(gate(&existing), GateOutcome::Confirm { .. }),
        "an existing temp file still requires fresh approval"
    );

    let absent_tree = scratch.path().join("absent-tree");
    let recursive = assess_command(
        &format!("rm -rf '{}'", absent_tree.display()),
        ShellSyntax::Bash,
        &context,
    )
    .expect("the command must parse");
    assert!(
        matches!(gate(&recursive), GateOutcome::Confirm { .. }),
        "recursive deletion must not inherit the narrow absent-file exemption"
    );
}

#[derive(Default)]
struct RecordingDenial {
    asks: Mutex<Vec<PermissionAsk>>,
}

#[async_trait::async_trait]
impl PermissionAsker for RecordingDenial {
    async fn ask(
        &self,
        _origin: zuno_tool::PermissionOrigin<'_>,
        tool: &str,
        ask: PermissionAsk,
    ) -> Result<(), ToolError> {
        self.asks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ask);
        Err(ToolError::Denied {
            tool: tool.to_owned(),
        })
    }
}

#[cfg(unix)]
#[tokio::test]
async fn risk_existing_redirect_target_requires_fresh_human_approval() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let target = root.path().join("existing.txt");
    std::fs::write(&target, b"keep").expect("existing target");
    let permission = Arc::new(RecordingDenial::default());
    let tool = ShellTool::new(&workspace).expect("shell tool");

    let error = tool
        .run(
            ShellParams {
                command: format!("printf replacement > '{}'", target.display()),
                timeout: None,
                workdir: None,
                background: false,
            },
            tool_context_with(permission.clone()),
        )
        .await
        .expect_err("the attached user denied the overwrite");

    assert!(
        matches!(error, ToolError::Denied { .. }),
        "the existing target must be protected by HITL: {error:?}"
    );
    assert_eq!(std::fs::read(&target).expect("target"), b"keep");
    let asks = permission
        .asks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(asks.len(), 1, "{asks:#?}");
    assert!(asks[0].manual, "the overwrite accepted an automatic grant");
    assert_eq!(asks[0].permission, "shell");
    let target_text = target.to_string_lossy().into_owned();
    assert_eq!(
        asks[0]
            .metadata
            .get("target")
            .and_then(serde_json::Value::as_str),
        Some(target_text.as_str())
    );
}

#[cfg(unix)]
#[tokio::test]
async fn risk_gate_runs_before_explicit_background_dispatch() {
    let directory = tempfile::tempdir().expect("temp directory");
    let target = directory.path().join("build");
    std::fs::create_dir(&target).expect("build directory");
    std::fs::write(target.join("sentinel"), b"keep").expect("sentinel");
    let tool = ShellTool::new(directory.path()).expect("shell tool");
    let permission = Arc::new(RecordingDenial::default());

    let error = tool
        .run(
            ShellParams {
                command: format!("rm -rf '{}'", target.display()),
                timeout: None,
                workdir: None,
                background: true,
            },
            tool_context_with(permission.clone()),
        )
        .await
        .expect_err("background execution must not bypass manual approval");

    assert!(matches!(error, ToolError::Denied { .. }));
    let asks = permission
        .asks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(asks.len(), 1, "{asks:#?}");
    assert!(asks[0].manual);
    assert!(target.join("sentinel").is_file());
    assert!(!directory.path().join(".zuno/background").exists());
}

fn tool_context_with(permission: Arc<dyn PermissionAsker>) -> ToolContext {
    ToolContext::new(
        "ses_risk",
        "msg_risk",
        "call_risk",
        "build",
        permission,
        Arc::new(NeverInterrupted),
    )
}

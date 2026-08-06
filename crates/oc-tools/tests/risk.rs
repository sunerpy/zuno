use oc_tool::{AllowAll, NeverInterrupted, ToolContext};
use oc_tools::risk::{GateOutcome, Justification, RiskContext, assess_command, gate};
use oc_tools::shell::{ShellParams, ShellSyntax, ShellTool};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn risk_context() -> RiskContext {
    RiskContext {
        working_dir: Some(PathBuf::from("/work/project")),
        home_dir: Some(PathBuf::from("/home/alice")),
    }
}

fn risk_gate(command: &str, justification: Option<&str>) -> GateOutcome {
    let assessment = assess_command(command, ShellSyntax::Bash, &risk_context())
        .expect("the command must parse");
    gate(
        &assessment,
        &Justification {
            text: justification.map(str::to_owned),
        },
    )
}

fn assert_risk_denied(command: &str) {
    let outcome = risk_gate(command, None);
    assert!(
        matches!(outcome, GateOutcome::Deny { .. }),
        "expected permanent denial for {command:?}, got {outcome:?}"
    );
    assert!(
        matches!(
            risk_gate(
                command,
                Some("The user explicitly requested this exact destructive target")
            ),
            GateOutcome::Deny { .. }
        ),
        "a justification must never unlock catastrophic target {command:?}"
    );
}

fn assert_risk_denied_with_syntax(command: &str, syntax: ShellSyntax) {
    let assessment =
        assess_command(command, syntax, &risk_context()).expect("the command must parse");
    for justification in [
        None,
        Some("The user explicitly requested this exact destructive target"),
    ] {
        let outcome = gate(
            &assessment,
            &Justification {
                text: justification.map(str::to_owned),
            },
        );
        assert!(
            matches!(outcome, GateOutcome::Deny { .. }),
            "expected permanent denial for {command:?} under {syntax:?}, got {outcome:?} from {assessment:#?}"
        );
    }
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
        risk_gate("rm -rf ~other", None),
        GateOutcome::Reflect { .. }
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
    assert_eq!(risk_gate("foo > /dev/null", None), GateOutcome::Allow);
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
        risk_gate("rm -rf /tmp/{1..3}", None),
        GateOutcome::Reflect { .. }
    ));
}

#[test]
fn risk_reflection_requires_more_than_a_blind_retry_or_bare_yes() {
    let first = risk_gate("rm -rf ./build", None);
    let GateOutcome::Reflect { prompt } = first else {
        panic!("a bounded destructive command must require reflection");
    };
    assert!(prompt.contains("Which specific thing the user asked for requires deleting this?"));
    assert!(prompt.contains("Did the user name this path, or did you infer it?"));

    assert!(matches!(
        risk_gate("rm -rf ./build", None),
        GateOutcome::Reflect { .. }
    ));
    assert!(matches!(
        risk_gate("rm -rf ./build", Some("yes")),
        GateOutcome::Reflect { .. }
    ));
    assert_eq!(
        risk_gate(
            "rm -rf ./build",
            Some("The user asked to remove the generated build directory before rebuilding")
        ),
        GateOutcome::Allow
    );
}

#[test]
fn risk_unknown_dynamic_delete_target_reflects_instead_of_guessing() {
    for command in [
        "rm -rf \"$UNSET_VAR/\"",
        "find \"$TARGET\" -delete",
        "printf '%s' target | xargs rm -rf",
    ] {
        assert!(
            matches!(risk_gate(command, None), GateOutcome::Reflect { .. }),
            "unknown runtime target must be held: {command:?}"
        );
    }
    let dynamic_command = risk_gate("$DELETE_COMMAND -rf /", None);
    assert!(
        matches!(dynamic_command, GateOutcome::Reflect { .. }),
        "dynamic command names must reflect, got {dynamic_command:?}"
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
        assert_eq!(risk_gate(command, None), GateOutcome::Allow);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn risk_gate_runs_before_explicit_background_dispatch() {
    let directory = tempfile::tempdir().expect("temp directory");
    let target = directory.path().join("build");
    std::fs::create_dir(&target).expect("build directory");
    std::fs::write(target.join("sentinel"), b"keep").expect("sentinel");
    let tool = ShellTool::new(directory.path()).expect("shell tool");

    let error = tool
        .run(
            ShellParams {
                command: format!("rm -rf '{}'", target.display()),
                timeout: None,
                workdir: None,
                background: true,
                justification: None,
            },
            tool_context(directory.path()),
        )
        .await
        .expect_err("background execution must not bypass reflection");

    let source = std::error::Error::source(&error).expect("reflection reason");
    assert!(source.to_string().contains("This command was not run"));
    assert!(target.join("sentinel").is_file());
    assert!(!directory.path().join(".opencode/background").exists());
}

fn tool_context(_workspace: &Path) -> ToolContext {
    ToolContext::new(
        "ses_risk",
        "msg_risk",
        "call_risk",
        "build",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
}

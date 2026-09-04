mod support;

use std::path::PathBuf;
#[cfg(unix)]
use std::sync::{Arc, Mutex};
#[cfg(unix)]
use zuno_error::ToolError;
#[cfg(unix)]
use zuno_tool::{NeverInterrupted, PermissionAsk, PermissionAsker, ToolContext};
use zuno_tools::risk::{GateOutcome, RiskContext, assess_command, gate};
#[cfg(unix)]
use zuno_tools::shell::ShellParams;
use zuno_tools::shell::ShellSyntax;

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
        "env -S 'rm -rf /'",
        "env --split-string='rm -rf /'",
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
fn risk_git_history_rewrites_require_fresh_confirmation_and_head_precondition() {
    for command in [
        "git commit --amend --no-edit",
        "git rebase main",
        "git tag --force v1.0.0",
    ] {
        let assessment = assess_command(command, ShellSyntax::Bash, &risk_context())
            .expect("the command must parse");
        assert!(
            assessment.requires_expected_git_head(),
            "local rewrite did not require an exact HEAD guard: {command:?}"
        );
        assert!(
            matches!(gate(&assessment), GateOutcome::Confirm { .. }),
            "local rewrite did not require fresh approval: {command:?}"
        );
    }
    for command in [
        "git -C . commit --amend --no-edit",
        "git --git-dir=.git commit --amend --no-edit",
        "git --work-tree=. rebase main",
        "env GIT_DIR=.git git commit --amend --no-edit",
        "GIT_WORK_TREE=. git rebase main",
    ] {
        assert_risk_denied(command);
    }
    for command in [
        "git rebase --abort",
        "git rebase --continue",
        "git status --short",
    ] {
        let assessment = assess_command(command, ShellSyntax::Bash, &risk_context())
            .expect("the command must parse");
        assert!(!assessment.requires_expected_git_head(), "{command:?}");
    }
}

#[test]
fn risk_force_push_requires_an_explicit_atomic_remote_oid_lease() {
    let oid = "0123456789abcdef0123456789abcdef01234567";
    let guarded = risk_gate(&format!(
        "git push --force-with-lease=refs/heads/main:{oid} origin main"
    ));
    assert!(
        matches!(guarded, GateOutcome::Confirm { .. }),
        "an explicit lease still needs fresh approval: {guarded:?}"
    );

    for command in [
        "git push --force origin main",
        "git push -f origin main",
        "git push --force-with-lease origin main",
        "git push --force-with-lease=refs/heads/main origin main",
        "git push --force-with-lease=refs/heads/main:0123456789abcdef0123456789abcdef01234567 --force-with-lease origin main",
        "git push origin +main:main",
    ] {
        assert_risk_denied(command);
    }
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
fn risk_env_without_a_child_command_is_a_safe_environment_query() {
    for command in [
        "env",
        "env -0",
        "env --ignore-environment",
        "env SAFE=1",
        "env -i SAFE=1 OTHER=two",
        "env --unset SECRET SAFE=1",
        "env -u SECRET -- SAFE=1",
    ] {
        assert_eq!(
            risk_gate(command),
            GateOutcome::Allow,
            "environment-only invocation must not be mistaken for an unknown child command: {command:?}"
        );
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
        &format!("printf puzzle > '{}'", zuno_paths::wire_path(&target)),
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
        zuno_paths::wire_path(&target),
        zuno_paths::wire_path(&target),
        zuno_paths::wire_path(&target)
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
        &format!("rm -f '{}'", zuno_paths::wire_path(&target)),
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

#[cfg(unix)]
#[derive(Default)]
struct RecordingDenial {
    asks: Mutex<Vec<PermissionAsk>>,
}

#[cfg(unix)]
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
    let tool = support::sandbox::shell_tool(&workspace);

    let error = tool
        .run(
            ShellParams {
                command: format!("printf replacement > '{}'", target.display()),
                timeout: None,
                workdir: None,
                background: false,
                background_purpose: zuno_pty::BackgroundExecutionPurpose::Command,
                expected_git_head: None,
                exit_policy: None,
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
    let tool = support::sandbox::shell_tool(directory.path());
    let permission = Arc::new(RecordingDenial::default());

    let error = tool
        .run(
            ShellParams {
                command: format!("rm -rf '{}'", target.display()),
                timeout: None,
                workdir: None,
                background: true,
                background_purpose: zuno_pty::BackgroundExecutionPurpose::Command,
                expected_git_head: None,
                exit_policy: None,
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

#[cfg(unix)]
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

/// A Windows host's context: `HOME` is unset there, so [`RiskContext::from_env`]
/// takes the profile directory from `std::env::home_dir`, which reads `USERPROFILE`.
fn windows_risk_context() -> RiskContext {
    RiskContext {
        working_dir: Some(PathBuf::from(r"C:\work\project")),
        home_dir: Some(PathBuf::from(r"C:\Users\alice")),
    }
}

fn windows_risk_gate(command: &str) -> GateOutcome {
    let assessment = assess_command(command, ShellSyntax::PowerShell, &windows_risk_context())
        .expect("the command must parse");
    gate(&assessment)
}

fn assert_windows_denied(command: &str) {
    let outcome = windows_risk_gate(command);
    assert!(
        matches!(outcome, GateOutcome::Deny { .. }),
        "expected permanent denial for {command:?}, got {outcome:?}"
    );
}

#[test]
fn risk_denies_windows_system_locations_in_every_spelling() {
    for target in [
        r"C:\",
        r"C:\Windows",
        "C:/Windows",
        r"c:\windows\system32",
        r"C:\Program Files",
        r"C:\Program Files (x86)\Vendor",
        r"C:\ProgramData\Vendor",
        r"D:\Windows",
        r"\\?\C:\Windows",
        r"\\server\share",
    ] {
        assert_windows_denied(&format!("Remove-Item -Recurse -Force '{target}'"));
    }
}

#[test]
fn risk_denies_the_windows_profile_and_its_credential_stores() {
    for target in [
        "~",
        "$HOME",
        "${HOME}",
        r"C:\Users\alice",
        r"C:\Users",
        r"C:\Users\alice\.ssh",
        r"C:\Users\alice\.ssh\id_rsa",
        r"C:\Users\alice\.aws",
        r"C:\Users\alice\.config\gh",
        r"C:\Users\alice\Documents",
        "~/.ssh",
    ] {
        assert_windows_denied(&format!("Remove-Item -Recurse -Force '{target}'"));
    }
}

#[test]
fn risk_denies_the_windows_profile_through_the_spellings_its_own_shells_use() {
    // `%USERPROFILE%` and `$env:USERPROFILE` are how `cmd` and PowerShell name the
    // profile directory; neither shell sets `HOME`, so before the home fallback all
    // of these resolved to nothing and dropped to a confirmation prompt.
    for target in [
        "%USERPROFILE%",
        r"%USERPROFILE%\.ssh",
        r"%UserProfile%\.ssh",
        "$env:USERPROFILE",
        r"$env:USERPROFILE\.ssh",
        r"${env:userprofile}\.gnupg",
    ] {
        assert_windows_denied(&format!("Remove-Item -Recurse -Force '{target}'"));
    }
}

#[test]
fn risk_still_confirms_ordinary_windows_project_work() {
    for target in [
        r"C:\work\project\target",
        r"C:\Users\alice\project",
        r"C:\Users\alice\.config\zuno-scratch",
        r"\\server\share\project",
        "build",
    ] {
        let command = format!("Remove-Item -Recurse -Force '{target}'");
        let outcome = windows_risk_gate(&command);
        assert!(
            matches!(outcome, GateOutcome::Confirm { .. }),
            "expected a confirmable operation for {command:?}, got {outcome:?}"
        );
    }
}

#[test]
fn risk_reads_a_backslash_as_a_separator_under_powershell_and_an_escape_under_bash() {
    // A backslash is a path separator in PowerShell and an escape in Bash. Reducing a
    // PowerShell token with the POSIX rule turned every absolutely spelled program
    // into a name that matched no table, so a nested shell and a destructive program
    // both escaped assessment; reading it as a separator under Bash would let `r\m`
    // through instead. Both spellings have to stay denied.
    assert_windows_denied(r"C:\Tools\bash.exe -c 'rm -rf /'");
    assert_windows_denied(r"rm.exe -rf C:\Users\alice\.ssh");
    assert_windows_denied(r"C:\Windows\System32\format.com C:");
    assert_risk_denied(r"r\m -rf /");
    // A backtick is PowerShell's escape character rather than Bash's command
    // substitution, so `C:\Wind`ows` in fact names `C:\Windows`. The assessor still
    // reflects it as runtime-computed and requires confirmation instead of denying it:
    // a documented over-approximation, pinned here so it stays a prompt and never
    // becomes silence.
    let outcome = windows_risk_gate("Remove-Item -Recurse -Force C:\\Wind`ows");
    assert!(
        matches!(outcome, GateOutcome::Confirm { .. }),
        "a backtick target must still reach the user: {outcome:?}"
    );
}

/// Bash's `$'…'` (ANSI-C) and `$"…"` (locale) quoting denote their body literally when
/// no escape is inside, so every one of these runs `rm -rf /`. Read as ordinary
/// characters the program was `rm$`, which is in no table, and the call was `Allow`.
#[test]
fn risk_dollar_quoting_cannot_hide_a_catastrophic_program_or_target() {
    for command in [
        "rm$'' -rf /",
        "r$'m' -rf /",
        "$'rm' -rf /",
        "rm$\"\" -rf /",
        "r$\"m\" -rf /",
        "env rm$'' -rf /",
        "sudo -u root rm$'' -rf /",
        "nice -n 5 r$'m' -rf /",
        "rm -rf $'/'",
        "sh -c $'rm -rf /'",
        "bash -c $\"rm -rf /\"",
    ] {
        assert_risk_denied(command);
    }
}

/// A `$`, a backtick or a glob character anywhere in the program word — not only at
/// its first character — means the shell computes the program, and the gate can only
/// ask a human. `rm${IFS}-rf${IFS}/` and `git${IFS}push --force` were `Allow`.
#[test]
fn risk_a_program_computed_at_runtime_is_held_wherever_the_expansion_sits() {
    for command in [
        "rm${IFS}-rf${IFS}/",
        "rm$IFS-rf$IFS/",
        "env rm${IFS}-rf${IFS}/",
        "sudo rm${IFS}-rf${IFS}/",
        "git${IFS}push --force",
        "git $SUB --force",
        "$(echo rm) -rf /",
        "`echo rm` -rf /",
        "/usr/bin/r? -rf /",
        "/usr/bin/r* -rf /",
        "r[m] -rf /",
    ] {
        let outcome = risk_gate(command);
        assert!(
            matches!(outcome, GateOutcome::Confirm { ref reason, .. }
                if reason.contains("cannot be checked")),
            "a computed program must be held for a human, not allowed: {command:?} -> {outcome:?}"
        );
    }
}

/// A `$'…'` body is materialised, escapes and all, so the word names what the shell will
/// name. Left as written its `$` made the word merely computed: `r$'\x6d' -rf /` was a
/// prompt for an unknown program and `sh -c $'echo hi\nrm -rf /'` a prompt for an unknown
/// script, while both are exactly `rm -rf /`.
#[test]
fn risk_ansi_c_quoting_is_materialised_before_the_program_is_judged() {
    for command in [
        "r$'\\x6d' -rf /",
        "sh -c $'echo hi\\nrm -rf /'",
        "sh -c $'rm\\x20-rf\\x20/'",
        "eval $'echo hi\\nrm -rf /'",
        "sudo sh -c $'echo hi\\nrm -rf /'",
    ] {
        assert_risk_denied(command);
    }
    // One word, spaces and all: `$'rm\x20-rf'` materialises to a program name with a
    // space in it. Bash reports `command not found` for that exact word, but the same
    // shape also comes out of the tokenizer for a line that really runs `rm -rf /`
    // (`$'echo hi'\;$'rm -rf /'` is one shell word split into two tokens), so a
    // materialised program that is not one name is held for a human, never waved through.
    assert!(matches!(
        risk_gate("$'rm\\x20-rf' /"),
        GateOutcome::Confirm { .. }
    ));
    for command in [r"sh -c $'echo hi'\;$'rm -rf /'", "$'rm -rf' /"] {
        assert!(
            !matches!(risk_gate(command), GateOutcome::Allow),
            "{command} must not run without a prompt"
        );
    }
}

/// The subcommand and the options of a git call get the shell's own per-character
/// reading, so `p''ush` is `push` and `--for''ce` is `--force`. Both were `Allow`.
#[test]
fn risk_git_spelling_tricks_still_reach_the_force_push_gate() {
    for command in [
        "git p''ush --force",
        "git push --for''ce",
        "git 'push' -f",
        "git p\"ush\" --force",
        "git push --f'o'rce",
        "git p$'ush' --force",
        "git --no-pager p''ush -f origin main",
    ] {
        let outcome = risk_gate(command);
        assert!(
            matches!(outcome, GateOutcome::Deny { ref reason }
                if reason.contains("published Git history")),
            "expected the force-push denial for {command:?}, got {outcome:?}"
        );
    }
    assert!(
        matches!(
            risk_gate(
                "git push --force-with-lease=refs/heads/main:0123456789abcdef0123456789abcdef01234567"
            ),
            GateOutcome::Confirm { .. }
        ),
        "an explicit atomic lease is still confirmable, not denied"
    );
    assert_eq!(risk_gate("git p''ush origin main"), GateOutcome::Allow);
}

/// A PowerShell `?` is the `Where-Object` alias, a fixed cmdlet, so the wider dynamic
/// reading must not start prompting on an ordinary pipeline.
#[test]
fn risk_powershell_where_object_alias_is_not_a_glob() {
    let assessment = assess_command(
        "Get-Process | ? { $_.CPU -gt 10 }",
        ShellSyntax::PowerShell,
        &risk_context(),
    )
    .expect("the command must parse");
    assert_eq!(gate(&assessment), GateOutcome::Allow);
}

/// A word the shell computes, sitting between a wrapper and its program, can only add
/// uncertainty. It must never remove a denial the static words already justify: with
/// `EMPTY` unset every one of these runs `rm -rf /` (or force-pushes), and each was a
/// human-approvable Confirm because the computed word ended the assessment.
#[test]
fn risk_a_computed_word_before_the_program_cannot_downgrade_a_denial() {
    for command in [
        "env $FOO rm -rf /",
        "env ${FOO} rm -rf /",
        "sudo $U rm -rf /",
        "nice $X rm -rf /",
        "timeout $T rm -rf /",
        "sudo -u root $EMPTY rm -rf /",
        "sudo -E$FLAGS rm -rf /",
        "sh -c $EMPTY 'rm -rf /'",
        "bash -c $EMPTY $ALSO_EMPTY 'rm -rf /'",
        "git $EMPTY push --force",
        "$FOO rm -rf /",
        "chroot /mnt $X rm -rf /",
    ] {
        assert_risk_denied(command);
    }
    // The uncertainty itself is still reported next to the denial.
    let outcome = risk_gate("sudo -u root $EMPTY rm -rf /");
    assert!(
        matches!(outcome, GateOutcome::Deny { ref reason }
            if reason.contains("computed at runtime") && reason.contains("protected system")),
        "both the computed word and the catastrophic target must be explained: {outcome:?}"
    );
}

/// A wrapper option the gate does not know must not decide which word is the program.
/// `exec -a foo rm -rf /` was the program `foo` — `-a` was read as a flag, `foo` was
/// judged and found harmless, and `rm` was never examined — so the call was Allow with
/// no prompt at all. The same happened for every real value-taking option missing from
/// the table, for a short-option cluster, and for a truly unknown option.
#[test]
fn risk_a_wrapper_option_the_gate_does_not_know_cannot_hide_the_program_behind_it() {
    for command in [
        "exec -a foo rm -rf /",
        "watch -n 1 rm -rf /",
        "watch --interval 1 rm -rf /",
        r"xargs -d '\n' rm -rf /",
        "xargs --replace rm -rf /",
        "sudo -R / rm -rf /",
        "sudo -D /tmp rm -rf /",
        "sudo -T 5 rm -rf /",
        "sudo -r role rm -rf /",
        "sudo -t type rm -rf /",
        "sudo -U alice rm -rf /",
        "sudo -Eu root rm -rf /",
        "sudo --zuno-unknown-option value rm -rf /",
        "sudo --zuno-unknown-option rm -rf /",
        "sudo -Z value rm -rf /",
        "ionice -t rm -rf /",
        "flock /tmp/lock rm -rf /",
        "flock -w 5 /tmp/lock rm -rf /",
        "flock /tmp/lock -c 'rm -rf /'",
        "taskset 0x3 rm -rf /",
        "taskset -c 0-3 rm -rf /",
        "chrt -f 99 rm -rf /",
        "sh -c -x 'rm -rf /'",
        "exec -a foo watch -n 1 sudo -R / rm -rf /",
    ] {
        assert_risk_denied(command);
    }
}

/// The fail-closed reading must not start prompting on the wrapper options people
/// actually write, and the verdicts the gate already gave must stand.
#[test]
fn risk_known_wrapper_options_keep_their_verdicts() {
    for command in [
        "sudo -u root ls",
        "sudo -E -H -u root ls -la",
        "sudo --user=root ls",
        "sudo -uroot ls",
        "sudo -Eu root ls",
        "sudo -R / ls",
        "sudo -n true",
        "env FOO=bar make",
        "env -i FOO=bar make",
        "timeout 5 cargo test",
        "timeout -k 5 10s cargo test",
        "exec -a foo ls",
        "watch -n 1 date",
        "watch -d -n 1 date",
        "xargs -0 -n 1 echo",
        "xargs -I {} echo {}",
        "nice -n 5 make",
        "nice -5 make",
        "stdbuf -oL cat",
        "ionice -c 3 -n 7 cat",
        "ionice -t cat",
        "chroot /mnt ls",
        "taskset 0x3 ls",
        "chrt -f 99 ls",
        "flock /tmp/lock ls",
        "flock -n 9",
        "sudo --zuno-unknown-option value ls",
    ] {
        assert_eq!(risk_gate(command), GateOutcome::Allow, "{command:?}");
    }
    for command in ["sudo -u $USER rm -rf /", "nice -n $N rm -rf /"] {
        assert_risk_denied(command);
    }
    for command in [
        // An option the gate does not know may have taken the program as its value.
        "sudo --zuno-unknown-option ls",
        // A destructive program behind an unknown option is still bounded.
        "exec -Z rm -rf ./build",
        "sudo -s",
        "env rm${IFS}-rf${IFS}/",
        "$DELETE_COMMAND -rf /",
    ] {
        let outcome = risk_gate(command);
        assert!(
            matches!(outcome, GateOutcome::Confirm { .. }),
            "expected a confirmation for {command:?}, got {outcome:?}"
        );
    }
}

/// A program path with a space in its directory is one name, not a command line, and a
/// quoted script that materialises to `rm -rf` is a command line, not a name.
#[test]
fn risk_a_quoted_program_path_with_a_space_is_one_name() {
    for command in [
        r#""C:\Program Files\Git\cmd\git.exe" --version"#,
        "'/opt/my app/bin/tool' --version",
        "'/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code' .",
        "/opt/my\\ app/bin/tool --version",
    ] {
        assert_eq!(risk_gate(command), GateOutcome::Allow, "{command}");
    }
    // A directory whose name starts with a program name does not turn the path into a
    // command line: the whole word is the program `rm`, judged as `rm`.
    for command in [
        r#""/opt/sh dir/rm" -rf /"#,
        r#""/usr/local/sudo bin/rm" -rf /"#,
        r#""/home/alice/git repos/rm" -rf /"#,
        r#""/opt/find tools/sh" -c 'rm -rf /'"#,
        r#""/opt/rm bin/dd" of=/dev/sda"#,
    ] {
        assert_risk_denied(command);
    }
    for command in ["$'rm -rf' /", "$'sudo rm' -rf /", "$'sh -c' 'rm -rf /'"] {
        assert!(
            !matches!(risk_gate(command), GateOutcome::Allow),
            "{command} must not run without a prompt"
        );
    }
    // The same split shape behind `su -c`: the argument is held too.
    assert!(!matches!(
        risk_gate(r"su -c 'echo hi'\;'rm -rf /'"),
        GateOutcome::Allow
    ));
}

/// Every computed word after a wrapper forks a reading at every later word, so a line of
/// hundreds of computed words was cubic work: four hundred took seconds and three
/// thousand did not finish. Past a few thousand walk states the walk stops and the line
/// is held for a human; a line within the cap is still refused outright.
#[test]
fn risk_many_computed_words_after_a_wrapper_are_held_rather_than_read_forever() {
    assert_risk_denied(&format!(
        "env {} rm -rf /",
        (0..8)
            .map(|i| format!("$A{i}"))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    // A full queue stops forking, never the reading being followed: the chain of
    // computed assignments still reaches `rm -rf /` and is refused.
    assert_risk_denied(&format!(
        "env {} rm -rf /",
        (0..100)
            .map(|i| format!("V{i}=$X{i}"))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    // A script option that is really on the line is never starved by speculative
    // readings from a long tail: with `A` unset this runs `rm -rf / f0 …`.
    assert_risk_denied(&format!(
        "env $A -S 'rm -rf /' {}",
        (0..300)
            .map(|i| format!("f{i}"))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    for n in [2000usize, 3000] {
        let words = (0..n)
            .map(|i| format!("$A{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let started = std::time::Instant::now();
        let outcome = risk_gate(&format!("env {words} rm -rf /"));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "the wrapper walk must stop forking at n={n}: {:?}",
            started.elapsed()
        );
        assert!(
            !matches!(outcome, GateOutcome::Allow),
            "a line the walk could not finish reading is held, never waved through: {outcome:?}"
        );
    }
}

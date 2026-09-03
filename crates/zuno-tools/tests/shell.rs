mod support;

#[cfg(unix)]
use async_trait::async_trait;
#[cfg(unix)]
use serde_json::json;
#[cfg(unix)]
use sha2::{Digest as _, Sha256};
#[cfg(unix)]
use std::collections::BTreeMap;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::process::Command;
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(unix)]
use std::time::{Duration, Instant};
#[cfg(unix)]
use tokio::sync::Notify;
#[cfg(unix)]
use zuno_error::ToolError;
use zuno_permission::{PermissionAction, Rule, evaluate};
#[cfg(unix)]
use zuno_pty::BackgroundExecutionPurpose;
use zuno_tool::Tool;
#[cfg(unix)]
use zuno_tool::{
    ACCEPT_LARGE_OUTPUT_KEY, AllowAll, InterruptHandle, NeverInterrupted, ToolContext,
};
#[cfg(unix)]
use zuno_tool::{
    ExitAuthority, OutputLimits, ReceiptOutcome, ToolOutput, ToolOutputStore,
    VERIFICATION_METADATA_KEY, VerificationReceipt,
};
#[cfg(unix)]
use zuno_tools::shell::{ExitPolicy, ShellEnvHook, ShellEnvInput, ShellParams};
use zuno_tools::shell::{ShellSyntax, analyze_command};

#[cfg(unix)]
#[derive(Default)]
struct FirableInterrupt {
    fired: AtomicBool,
    notify: Notify,
}

#[cfg(unix)]
impl FirableInterrupt {
    fn fire(&self) {
        self.fired.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

#[cfg(unix)]
#[async_trait]
impl InterruptHandle for FirableInterrupt {
    fn is_set(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }

    async fn notified(&self) {
        while !self.is_set() {
            self.notify.notified().await;
        }
    }
}

#[cfg(unix)]
fn context(interrupt: Arc<dyn InterruptHandle>) -> ToolContext {
    ToolContext::new(
        "ses_shell",
        "msg_shell",
        "call_shell",
        "build",
        Arc::new(AllowAll),
        interrupt,
    )
}

#[cfg(unix)]
fn params(command: impl Into<String>) -> ShellParams {
    ShellParams {
        command: command.into(),
        timeout: None,
        workdir: None,
        background: false,
        background_purpose: BackgroundExecutionPurpose::Command,
        expected_git_head: None,
        exit_policy: None,
    }
}

/// `/bin/sh` is `dash` on Debian, and `dash` rejects `set -o pipefail` outright,
/// so the end-to-end policy runs pin the POSIX interpreter that every supported
/// platform ships and that does implement the option.
#[cfg(unix)]
const PIPEFAIL_SHELL: &str = "/bin/bash";

#[cfg(unix)]
fn policy_params(command: &str, policy: ExitPolicy) -> ShellParams {
    ShellParams {
        exit_policy: Some(policy),
        ..params(command)
    }
}

#[cfg(unix)]
fn receipt(output: &ToolOutput) -> VerificationReceipt {
    VerificationReceipt::from_metadata(&output.metadata)
        .expect("the host must be able to decode the receipt")
        .expect("every shell result carries a receipt")
}

#[cfg(unix)]
fn git(workspace: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git stdout")
        .trim()
        .to_owned()
}

#[cfg(unix)]
fn initialize_git_repository(workspace: &Path) -> String {
    git(workspace, &["init", "--quiet"]);
    git(workspace, &["config", "user.name", "Zuno Test"]);
    git(workspace, &["config", "user.email", "zuno@example.invalid"]);
    std::fs::write(workspace.join("tracked.txt"), b"initial\n").expect("tracked file");
    git(workspace, &["add", "tracked.txt"]);
    git(workspace, &["commit", "--quiet", "-m", "initial"]);
    git(workspace, &["rev-parse", "HEAD"])
}

/// Where git is, spelled absolutely, the way a script or a Windows resolver spells it.
#[cfg(unix)]
fn git_program() -> std::path::PathBuf {
    let output = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("locate git");
    assert!(output.status.success(), "git must be on PATH for this test");
    let located = String::from_utf8(output.stdout).expect("utf-8 path");
    std::path::PathBuf::from(located.trim())
}

#[cfg(unix)]
struct RedirectGitRepository {
    git_dir: String,
}

#[cfg(unix)]
#[async_trait]
impl ShellEnvHook for RedirectGitRepository {
    async fn env(&self, _input: ShellEnvInput) -> Result<BTreeMap<String, String>, ToolError> {
        Ok(BTreeMap::from([(
            "GIT_DIR".to_owned(),
            self.git_dir.clone(),
        )]))
    }
}

/// A commit that would deliver Zuno's own working state is refused before it runs.
///
/// Goal documents live inside the worktree so a person can read where a run stands.
/// Committing one puts runtime residue in the repository, where the next session reads
/// it as project source and reasons from it with the confidence that git lends.
#[cfg(unix)]
#[tokio::test]
async fn shell_refuses_a_commit_that_would_deliver_generated_state() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let head = initialize_git_repository(workspace.path());
    let tool = support::sandbox::shell_tool(workspace.path());

    let goal = workspace.path().join(".zuno").join("goal");
    std::fs::create_dir_all(&goal).expect("goal directory");
    std::fs::write(goal.join("ses_1.md"), b"# Objective\n").expect("goal document");
    // Forced, because the repository-private exclude block is what normally keeps this
    // path out of the index. The refusal exists for when that block is gone or bypassed.
    git(workspace.path(), &["add", "--force", ".zuno/goal/ses_1.md"]);

    let refusal = tool
        .run(
            params("git commit --quiet -m deliver"),
            context(Arc::new(NeverInterrupted)),
        )
        .await
        .expect_err("a staged goal document must not reach a commit");
    let rendered = format!("{refusal:?}");
    assert!(rendered.contains(".zuno/goal/ses_1.md"), "{rendered}");
    assert!(rendered.contains("goal projection"), "{rendered}");
    assert!(rendered.contains("git restore --staged"), "{rendered}");
    assert_eq!(
        git(workspace.path(), &["rev-parse", "HEAD"]),
        head,
        "the refusal must happen before the commit"
    );

    git(
        workspace.path(),
        &["restore", "--staged", "--", ".zuno/goal/ses_1.md"],
    );
    std::fs::write(workspace.path().join("tracked.txt"), b"edited\n").expect("source edit");
    tool.run(
        params("git commit --quiet -a -m source"),
        context(Arc::new(NeverInterrupted)),
    )
    .await
    .expect("an ordinary source commit is untouched");
    assert_ne!(
        git(workspace.path(), &["rev-parse", "HEAD"]),
        head,
        "the source commit must have landed"
    );
}

/// `git commit -a` stages tracked changes itself, so the index alone is not the delivery.
///
/// A generated path that is already tracked — committed once before the exclude block
/// existed — is delivered again by every `-a` commit that follows, and nothing in the
/// index would show it.
#[cfg(unix)]
#[tokio::test]
async fn shell_reads_the_worktree_when_a_commit_stages_tracked_changes_itself() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    initialize_git_repository(workspace.path());
    let tool = support::sandbox::shell_tool(workspace.path());

    let output = workspace.path().join(".zuno").join("tool-output");
    std::fs::create_dir_all(&output).expect("tool output directory");
    let spill = output.join("call_1.txt");
    std::fs::write(&spill, b"first\n").expect("spilled output");
    git(
        workspace.path(),
        &["add", "--force", ".zuno/tool-output/call_1.txt"],
    );
    git(workspace.path(), &["commit", "--quiet", "-m", "residue"]);
    let head = git(workspace.path(), &["rev-parse", "HEAD"]);
    std::fs::write(&spill, b"second\n").expect("spilled output again");

    let refusal = tool
        .run(
            params("git commit --quiet -am residue"),
            context(Arc::new(NeverInterrupted)),
        )
        .await
        .expect_err("a tracked spill must not be delivered again");
    let rendered = format!("{refusal:?}");
    assert!(
        rendered.contains(".zuno/tool-output/call_1.txt"),
        "{rendered}"
    );
    assert_eq!(
        git(workspace.path(), &["rev-parse", "HEAD"]),
        head,
        "the refusal must happen before the commit"
    );

    std::fs::write(workspace.path().join("tracked.txt"), b"edited\n").expect("source edit");
    git(workspace.path(), &["add", "tracked.txt"]);
    tool.run(
        params("git commit --quiet -m source"),
        context(Arc::new(NeverInterrupted)),
    )
    .await
    .expect("a commit of the index alone ignores the worktree's generated changes");
    assert_ne!(
        git(workspace.path(), &["rev-parse", "HEAD"]),
        head,
        "the source commit must have landed"
    );
}

/// Generated state is rooted at the worktree and hidden by the directory itself.
///
/// A session started in a subdirectory used to write `sub/.zuno/tool-output/`, which is
/// neither covered by the repository-private exclude block — anchored at the worktree
/// root — nor recognised by `classify`, so `git add -A` collected it and the delivery
/// check did not object. Two things have to hold at once: the directory lands at the
/// root, and it excludes itself as it is created, with no exclude block written here at
/// all.
#[cfg(unix)]
#[tokio::test]
async fn generated_state_from_a_session_in_a_subdirectory_is_rooted_and_hidden() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    initialize_git_repository(workspace.path());
    let root = workspace
        .path()
        .canonicalize()
        .expect("canonical workspace");
    let session = root.join("sub");
    std::fs::create_dir(&session).expect("session subdirectory");
    let tool = support::sandbox::shell_tool(&session).with_output_limits(OutputLimits {
        max_lines: 1,
        max_bytes: 4,
    });

    let output = tool
        .execute(
            json!({
                "command": "printf 'one\\ntwo\\n'",
                ACCEPT_LARGE_OUTPUT_KEY: true,
            }),
            context(Arc::new(NeverInterrupted)),
        )
        .await
        .expect("the command runs");

    let paths = output.output_paths();
    let stored = std::path::Path::new(paths.first().expect("stored output path"));
    assert!(
        stored.starts_with(root.join(".zuno").join("tool-output")),
        "{}",
        stored.display()
    );
    assert!(
        !session.join(".zuno").exists(),
        "the session's own directory is not where generated state goes"
    );
    for name in [".zuno/tool-output", ".zuno/background"] {
        let marker = root.join(name).join(".gitignore");
        assert_eq!(
            std::fs::read_to_string(&marker)
                .unwrap_or_else(|error| panic!("{}: {error}", marker.display()))
                .lines()
                .filter(|line| !line.starts_with('#') && !line.is_empty())
                .collect::<Vec<_>>(),
            vec!["*"],
            "{name} must exclude everything it holds"
        );
    }
    assert_eq!(
        git(&root, &["status", "--porcelain"]),
        "",
        "nothing wrote an exclude block here: the directories hide themselves"
    );
}

/// The exclusion is republished on every start, because the service recreates its root.
#[cfg(unix)]
#[tokio::test]
async fn a_deleted_background_directory_comes_back_excluded() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    initialize_git_repository(workspace.path());
    let root = workspace
        .path()
        .canonicalize()
        .expect("canonical workspace");
    let tool = support::sandbox::shell_tool(&root);
    tool.run(params("true"), context(Arc::new(NeverInterrupted)))
        .await
        .expect("the first command runs");
    let background = root.join(".zuno").join("background");
    std::fs::remove_dir_all(&background).expect("a person cleaning up");

    tool.run(params("true"), context(Arc::new(NeverInterrupted)))
        .await
        .expect("the second command runs");

    assert!(
        background.join(".gitignore").is_file(),
        "the recreated directory must be excluded again"
    );
    assert_eq!(git(&root, &["status", "--porcelain"]), "");
}

/// The program `git` is not the only way to spell git.
///
/// The delivery check compared the first token with `git`, so `/usr/bin/git commit` —
/// what a script writes, and what `git.exe` is on Windows — reached the commit without
/// ever reaching the check.
#[cfg(unix)]
#[tokio::test]
async fn a_path_qualified_git_commit_reaches_the_delivery_check() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let head = initialize_git_repository(workspace.path());
    let tool = support::sandbox::shell_tool(workspace.path());
    let goal = workspace.path().join(".zuno").join("goal");
    std::fs::create_dir_all(&goal).expect("goal directory");
    std::fs::write(goal.join("ses_1.md"), b"# Objective\n").expect("goal document");
    git(workspace.path(), &["add", "--force", ".zuno/goal/ses_1.md"]);

    let refusal = tool
        .run(
            params(format!(
                "{} commit --quiet -m deliver",
                git_program().display()
            )),
            context(Arc::new(NeverInterrupted)),
        )
        .await
        .expect_err("an absolutely spelled git is still git");

    assert!(
        format!("{refusal:?}").contains(".zuno/goal/ses_1.md"),
        "{refusal:?}"
    );
    assert_eq!(git(workspace.path(), &["rev-parse", "HEAD"]), head);
}

/// A commit that names its own repository is refused, not inspected.
///
/// `-C` was skipped as an option and its value discarded, so the check read the
/// repository the tool's `workdir` names while the commit wrote a different one. There
/// is no reading that fixes that: the repository has to be one fact both use.
#[cfg(unix)]
#[tokio::test]
async fn a_commit_that_selects_its_own_repository_is_refused() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let here = initialize_git_repository(workspace.path());
    let elsewhere = tempfile::tempdir().expect("other repository");
    let there = initialize_git_repository(elsewhere.path());
    std::fs::write(elsewhere.path().join("tracked.txt"), b"edited\n").expect("source edit");
    let tool = support::sandbox::shell_tool(workspace.path());

    for command in [
        format!(
            "git -C {} commit --quiet -am done",
            elsewhere.path().display()
        ),
        format!(
            "GIT_DIR={} git commit --quiet -am done",
            elsewhere.path().join(".git").display()
        ),
    ] {
        let refusal = tool
            .run(params(&command), context(Arc::new(NeverInterrupted)))
            .await
            .expect_err("a commit in another repository must not be admitted");
        let rendered = format!("{refusal:?}");
        assert!(
            rendered.contains("select the repository with the Shell workdir"),
            "{rendered}"
        );
        assert!(
            matches!(refusal, ToolError::InvalidArgs { .. }),
            "the arguments are what is wrong, and nothing ran: {refusal:?}"
        );
    }
    assert_eq!(git(workspace.path(), &["rev-parse", "HEAD"]), here);
    assert_eq!(git(elsewhere.path(), &["rev-parse", "HEAD"]), there);
}

/// `git add -A` collects nothing generated, whatever the delivery check reads.
///
/// The delivery check reads the reach of the staging it can see, and a Makefile target,
/// an alias, or a script the analyzer cannot see into has no reach it can read. What
/// holds without any reading is that each generated directory excludes itself as it is
/// created, with no repository-private exclude block written here at all — so an
/// untracked spill is invisible to `git add -A` in the first place, and the commit of
/// source goes through.
#[cfg(unix)]
#[tokio::test]
async fn staging_everything_collects_no_generated_state() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let head = initialize_git_repository(workspace.path());
    let root = workspace
        .path()
        .canonicalize()
        .expect("canonical workspace");
    let spilling = support::sandbox::shell_tool(&root).with_output_limits(OutputLimits {
        max_lines: 1,
        max_bytes: 4,
    });
    spilling
        .execute(
            json!({
                "command": "printf 'one\\ntwo\\n'",
                ACCEPT_LARGE_OUTPUT_KEY: true,
            }),
            context(Arc::new(NeverInterrupted)),
        )
        .await
        .expect("the command runs and spills its output");
    std::fs::write(root.join("source.txt"), b"written by the model\n").expect("source file");
    let tool = support::sandbox::shell_tool(&root);

    tool.run(
        params("git add -A && git commit --quiet -m wip"),
        context(Arc::new(NeverInterrupted)),
    )
    .await
    .expect("an ordinary commit of source");

    assert_ne!(
        git(&root, &["rev-parse", "HEAD"]),
        head,
        "the commit must have landed"
    );
    let committed = git(&root, &["show", "--pretty=", "--name-only", "HEAD"]);
    assert_eq!(committed, "source.txt", "{committed}");
    assert!(root.join(".zuno").join("tool-output").is_dir());
    assert!(root.join(".zuno").join("background").is_dir());
}

/// A chain that stages before it commits is read from the worktree, not from the index.
///
/// The check runs before the command, so at that moment `git add -A && git commit -m
/// wip` has staged nothing and the index says the commit is empty. The `add` is the
/// whole delivery, and its reach is readable: everything tracked, plus everything
/// untracked git does not ignore. This is the shape that matters, because no ignore rule
/// applies to a path git already tracks — a goal document an earlier release committed
/// is re-delivered by every `git add -A` until someone runs `git rm --cached` on it.
#[cfg(unix)]
#[tokio::test]
async fn a_chain_that_stages_before_it_commits_is_refused_for_tracked_generated_state() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    initialize_git_repository(workspace.path());
    let goal = workspace.path().join(".zuno").join("goal");
    std::fs::create_dir_all(&goal).expect("goal directory");
    std::fs::write(goal.join("ses_1.md"), b"# Objective\n").expect("goal document");
    git(workspace.path(), &["add", "--force", ".zuno/goal/ses_1.md"]);
    git(workspace.path(), &["commit", "--quiet", "-m", "residue"]);
    let head = git(workspace.path(), &["rev-parse", "HEAD"]);
    let tool = support::sandbox::shell_tool(workspace.path());
    // What the goal projection does every time the plan moves.
    std::fs::write(goal.join("ses_1.md"), b"# Objective\n\n- [x] step\n").expect("reprojected");

    for command in [
        "git add -A && git commit --quiet -m wip",
        "git add -u && git commit --quiet -m wip",
        "git add . && git commit --quiet -m wip",
        "git add .zuno && git commit --quiet -m wip",
        "git stage --update && git commit --quiet -m wip",
    ] {
        let refusal = tool
            .run(params(command), context(Arc::new(NeverInterrupted)))
            .await
            .expect_err("what the chain stages is part of the delivery");
        let rendered = format!("{refusal:?}");
        assert!(
            rendered.contains(".zuno/goal/ses_1.md"),
            "{command}: {rendered}"
        );
        assert!(
            rendered.contains("git rm --cached"),
            "{command}: {rendered}"
        );
        assert_eq!(
            git(workspace.path(), &["rev-parse", "HEAD"]),
            head,
            "{command}: the refusal must happen before the commit"
        );
    }

    // A narrower pathspec is a narrower read: the same chain limited to source stages no
    // generated state, so it commits.
    std::fs::write(workspace.path().join("tracked.txt"), b"edited\n").expect("source edit");
    tool.run(
        params("git add tracked.txt && git commit --quiet -m source"),
        context(Arc::new(NeverInterrupted)),
    )
    .await
    .expect("a chain that stages only source is untouched");
    let committed = git(
        workspace.path(),
        &["show", "--pretty=", "--name-only", "HEAD"],
    );
    assert_eq!(committed, "tracked.txt", "{committed}");
}

/// A `git add` after the last commit stages for the next call, not for this one.
///
/// Reading every `git add` in the command line regardless of order would refuse a commit
/// of source because a later `add` reaches generated state that this commit never
/// carried.
#[cfg(unix)]
#[tokio::test]
async fn staging_after_the_last_commit_does_not_refuse_it() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let head = initialize_git_repository(workspace.path());
    let goal = workspace.path().join(".zuno").join("goal");
    std::fs::create_dir_all(&goal).expect("goal directory");
    std::fs::write(goal.join("ses_1.md"), b"# Objective\n").expect("goal document");
    git(workspace.path(), &["add", "--force", ".zuno/goal/ses_1.md"]);
    git(workspace.path(), &["commit", "--quiet", "-m", "residue"]);
    std::fs::write(goal.join("ses_1.md"), b"# Objective\n\n- [x] step\n").expect("reprojected");
    std::fs::write(workspace.path().join("tracked.txt"), b"edited\n").expect("source edit");
    let tool = support::sandbox::shell_tool(workspace.path());

    tool.run(
        params("git add tracked.txt && git commit --quiet -m source && git add -A"),
        context(Arc::new(NeverInterrupted)),
    )
    .await
    .expect("the commit carries source only");

    let committed = git(
        workspace.path(),
        &["show", "--pretty=", "--name-only", "HEAD"],
    );
    assert_eq!(committed, "tracked.txt", "{committed}");
    assert_ne!(git(workspace.path(), &["rev-parse", "HEAD"]), head);
}

#[cfg(unix)]
#[tokio::test]
async fn shell_history_rewrite_requires_the_fresh_approved_head() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let original_head = initialize_git_repository(workspace.path());
    let tool = support::sandbox::shell_tool(workspace.path());

    let missing = tool
        .run(
            params("git commit --amend --no-edit"),
            context(Arc::new(NeverInterrupted)),
        )
        .await
        .expect_err("history rewrite without expectedGitHead must fail");
    assert!(
        format!("{missing:?}").contains("expectedGitHead is required"),
        "{missing:?}"
    );
    assert_eq!(git(workspace.path(), &["rev-parse", "HEAD"]), original_head);

    let mut stale = params("git commit --amend --no-edit");
    stale.expected_git_head = Some("0000000000000000000000000000000000000000".to_owned());
    let mismatch = tool
        .run(stale, context(Arc::new(NeverInterrupted)))
        .await
        .expect_err("stale expectedGitHead must fail");
    assert!(
        format!("{mismatch:?}").contains("Git HEAD changed"),
        "{mismatch:?}"
    );
    assert_eq!(git(workspace.path(), &["rev-parse", "HEAD"]), original_head);

    let mut guarded = params("git commit --amend --quiet -m amended");
    guarded.expected_git_head = Some(original_head.clone());
    tool.run(guarded, context(Arc::new(NeverInterrupted)))
        .await
        .expect("matching expectedGitHead admits the approved rewrite");
    assert_ne!(git(workspace.path(), &["rev-parse", "HEAD"]), original_head);
}

#[cfg(unix)]
#[tokio::test]
async fn shell_history_rewrite_refuses_a_repository_redirect_from_the_effective_environment() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let original_head = initialize_git_repository(workspace.path());
    let redirected = tempfile::tempdir().expect("redirected repository");
    initialize_git_repository(redirected.path());
    let tool = support::sandbox::shell_tool(workspace.path()).with_env_hook(Arc::new(
        RedirectGitRepository {
            git_dir: redirected.path().join(".git").display().to_string(),
        },
    ));
    let mut guarded = params("git commit --amend --quiet -m redirected");
    guarded.expected_git_head = Some(original_head.clone());

    let error = tool
        .run(guarded, context(Arc::new(NeverInterrupted)))
        .await
        .expect_err("the effective environment must not redirect a history rewrite");
    assert!(
        format!("{error:?}").contains("GIT_DIR may not be set"),
        "{error:?}"
    );
    assert_eq!(git(workspace.path(), &["rev-parse", "HEAD"]), original_head);
}

#[test]
fn shell_description_bounds_git_apply_and_defines_non_destructive_recovery() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let tool = support::sandbox::shell_tool(workspace.path());
    let description = tool.description();

    for clause in [
        "prefer native `apply_patch` or structured `edit`",
        "read every affected existing file",
        "`git apply --check`",
        "not a stale-read or rollback authority",
        "`--3way` or `--reject`",
        "`git reset --hard`",
        "`git checkout --`",
        "`backgroundPurpose: \"remoteObserver\"`",
        "re-query the authoritative remote state by stable identifier",
    ] {
        assert!(
            description.contains(clause),
            "shell description is missing `{clause}`:\n{description}"
        );
    }
}

#[test]
fn shell_schema_exposes_the_typed_remote_observer_purpose() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let tool = support::sandbox::shell_tool(workspace.path());
    let definition = tool.definition();
    let schema = definition.parameters.to_string();

    assert!(
        definition.parameters["properties"]
            .get("backgroundPurpose")
            .is_some(),
        "Shell omitted the backgroundPurpose field: {}",
        definition.parameters
    );
    assert!(
        schema.contains("remoteObserver"),
        "Shell did not publish the typed remoteObserver value: {schema}"
    );
}

#[test]
fn shell_compound_command_extracts_each_permission_resource_and_matches_real_rules() {
    let analysis =
        analyze_command("cd /tmp && git push origin main", ShellSyntax::Bash).expect("valid bash");
    let resources: Vec<&str> = analysis
        .commands
        .iter()
        .map(|resource| resource.source.as_str())
        .collect();

    assert_eq!(resources, vec!["cd /tmp", "git push origin main"]);
    assert_eq!(analysis.commands[1].always, "git push *");

    let rules = [
        Rule {
            permission: "shell".to_owned(),
            pattern: "*".to_owned(),
            action: PermissionAction::Deny,
        },
        Rule {
            permission: "shell".to_owned(),
            pattern: "git push*".to_owned(),
            action: PermissionAction::Ask,
        },
    ];
    assert_eq!(
        evaluate("shell", &analysis.commands[1].source, &rules),
        PermissionAction::Ask,
        "the extracted constituent, not the opaque compound string, must reach zuno-permission"
    );
    assert_eq!(
        evaluate("shell", &analysis.commands[0].source, &rules),
        PermissionAction::Deny,
        "the rule must distinguish cd from git push"
    );
}

#[test]
fn shell_analysis_of_command_substitution_never_executes_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let marker = dir.path().join("analysis-must-not-run");
    let command = format!("printf '%s' \"$(touch '{}')\"", marker.display());

    let analysis = analyze_command(&command, ShellSyntax::Bash).expect("valid substitution");

    assert!(
        !marker.exists(),
        "parsing shell text must be side-effect free"
    );
    assert!(
        analysis
            .commands
            .iter()
            .any(|resource| resource.source.starts_with("touch ")),
        "nested commands are permission resources too: {analysis:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn shell_cancellation_kills_the_shell_and_its_whole_process_group() {
    let dir = tempfile::tempdir().expect("temp dir");
    let parent_file = dir.path().join("parent.pid");
    let child_file = dir.path().join("child.pid");
    let command = format!(
        "printf '%s' \"$$\" > '{}'; sleep 30 & printf '%s' \"$!\" > '{}'; wait",
        parent_file.display(),
        child_file.display()
    );
    let interrupt = Arc::new(FirableInterrupt::default());
    let tool = support::sandbox::shell_tool(dir.path());
    let run_context = context(interrupt.clone());

    let running = tokio::spawn(async move { tool.run(params(command), run_context).await });
    wait_for_file(&parent_file).await;
    wait_for_file(&child_file).await;
    let parent = read_pid(&parent_file);
    let child = read_pid(&child_file);

    interrupt.fire();
    let error = tokio::time::timeout(Duration::from_secs(2), running)
        .await
        .expect("cancellation must finish promptly")
        .expect("task join")
        .expect_err("an interrupted command is not successful");

    assert!(matches!(error, ToolError::Failed { .. }));
    wait_for_process_exit(parent).await;
    wait_for_process_exit(child).await;
}

#[cfg(unix)]
#[tokio::test]
async fn shell_injected_hard_ceiling_really_terminates_the_process_under_four_seconds() {
    let dir = tempfile::tempdir().expect("temp dir");
    let pid_file = dir.path().join("ceiling.pid");
    let command = format!("printf '%s' \"$$\" > '{}'; sleep 30", pid_file.display());
    let tool = support::sandbox::shell_tool(dir.path()).with_hard_ceiling(Duration::from_secs(3));
    let started = Instant::now();

    // A saturated workspace test can delay the spawned shell for more than
    // 100 ms even though this fixture deliberately bypasses the production
    // sandbox backend. Observe the inner shell before asserting that the hard
    // ceiling kills it, so scheduler latency is not mistaken for leaked work.
    let running = tokio::spawn(async move {
        tool.run(
            params(command),
            context(Arc::new(zuno_tool::NeverInterrupted)),
        )
        .await
    });
    wait_for_file(&pid_file).await;
    let pid = read_pid(&pid_file);

    let error = tokio::time::timeout(Duration::from_secs(4), running)
        .await
        .expect("the hard ceiling must settle under four seconds")
        .expect("shell task joins")
        .expect_err("the hard ceiling must fail the call");

    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(matches!(error, ToolError::Timeout { .. }));
    wait_for_process_exit(pid).await;
}

#[cfg(unix)]
struct InjectEnv;

#[cfg(unix)]
#[async_trait]
impl ShellEnvHook for InjectEnv {
    async fn env(&self, input: ShellEnvInput) -> Result<BTreeMap<String, String>, ToolError> {
        assert_eq!(input.session_id, "ses_shell");
        assert_eq!(input.call_id, "call_shell");
        Ok(BTreeMap::from([(
            "ZUNO_T40_ENV".to_owned(),
            "injected".to_owned(),
        )]))
    }
}

#[cfg(unix)]
#[tokio::test]
async fn shell_env_hook_injects_call_scoped_environment() {
    let dir = tempfile::tempdir().expect("temp dir");
    let tool = support::sandbox::shell_tool(dir.path()).with_env_hook(Arc::new(InjectEnv));

    let output = tool
        .run(
            params("printf '%s' \"$ZUNO_T40_ENV\""),
            context(Arc::new(zuno_tool::NeverInterrupted)),
        )
        .await
        .expect("command succeeds");

    assert_eq!(output.output, "injected");
}

#[cfg(unix)]
#[tokio::test]
async fn configured_shell_identity_is_metadata_not_part_of_the_copyable_title() {
    let dir = tempfile::tempdir().expect("temp dir");
    let tool = support::sandbox::configured_shell_tool(dir.path(), Some("/bin/sh"));
    let definition = tool.definition();
    assert_eq!(definition.id, "shell", "wire id is platform-neutral");
    assert_eq!(definition.display_name, "sh");

    let output = tool
        .run(
            params("printf configured-shell"),
            context(Arc::new(zuno_tool::NeverInterrupted)),
        )
        .await
        .expect("command succeeds");

    assert_eq!(output.title, "printf configured-shell");
    assert_eq!(output.metadata["shell"], "sh");
    assert_eq!(output.output, "configured-shell");
}

#[cfg(unix)]
#[tokio::test]
async fn shell_background_mode_returns_before_the_command_finishes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let release = dir.path().join("release-background");
    let marker = dir.path().join("background-finished");
    let command = format!(
        "while [ ! -e '{}' ]; do sleep 0.01; done; touch '{}'",
        release.display(),
        marker.display()
    );
    let mut input = params(command.clone());
    input.background = true;
    let tool = support::sandbox::configured_shell_tool(dir.path(), Some("/bin/sh"));

    // Hosted runners can pause a correctly asynchronous spawn for hundreds of
    // milliseconds under load. The command waits on a test-owned gate instead,
    // so returning before the gate opens proves background dispatch directly.
    let output = match tokio::time::timeout(
        Duration::from_secs(5),
        tool.run(input, context(Arc::new(zuno_tool::NeverInterrupted))),
    )
    .await
    {
        Ok(result) => result.expect("background command starts"),
        Err(_) => {
            std::fs::write(&release, b"release\n").expect("release blocked background command");
            wait_for_file(&marker).await;
            panic!("background execution waited for the command to finish");
        }
    };

    assert!(
        !marker.exists(),
        "background execution returned only after the command finished"
    );
    assert_eq!(output.title, command);
    assert_eq!(output.metadata["shell"], "sh");
    assert_eq!(output.metadata["background"], true);
    assert_eq!(output.metadata["background_purpose"], "command");
    assert_eq!(output.metadata["requires_authoritative_refresh"], false);
    std::fs::write(&release, b"release\n").expect("release background command");
    wait_for_file(&marker).await;
}

#[cfg(unix)]
#[tokio::test]
async fn shell_marks_remote_observers_for_authoritative_refresh_after_wake() {
    let dir = tempfile::tempdir().expect("temp dir");
    let marker = dir.path().join("observer-finished");
    let command = format!("sleep 0.05; touch '{}'", marker.display());
    let mut input = params(command);
    input.background = true;
    input.background_purpose = BackgroundExecutionPurpose::RemoteObserver;
    let tool = support::sandbox::configured_shell_tool(dir.path(), Some("/bin/sh"));

    let output = tool
        .run(input, context(Arc::new(zuno_tool::NeverInterrupted)))
        .await
        .expect("remote observer starts");

    assert_eq!(output.metadata["background_purpose"], "remoteObserver");
    assert_eq!(output.metadata["requires_authoritative_refresh"], true);
    wait_for_file(&marker).await;
}

#[cfg(unix)]
#[tokio::test]
async fn shell_oversized_output_is_detected_and_persisted_in_the_shared_store() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store_dir = tempfile::tempdir().expect("store dir");
    let store = ToolOutputStore::new(store_dir.path());
    let tool = support::sandbox::shell_tool(dir.path())
        .with_output_store(store.clone())
        .with_output_limits(OutputLimits {
            max_lines: 1,
            max_bytes: 4,
        });

    let output = tool
        .execute(
            json!({
                "command": "printf 'one\\ntwo\\n'",
                ACCEPT_LARGE_OUTPUT_KEY: true,
            }),
            context(Arc::new(zuno_tool::NeverInterrupted)),
        )
        .await
        .expect("explicitly accepted command output succeeds");

    assert_eq!(output.output, "one\ntwo\n");
    assert_eq!(output.metadata["oversized"], true);
    let paths = output.output_paths();
    let path = paths.first().expect("stored output path");
    assert_eq!(
        store
            .read("shell", std::path::Path::new(path))
            .expect("stored full output"),
        output.output
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_failing_stage_in_a_pipeline_is_reported_as_a_failure() {
    let dir = tempfile::tempdir().expect("temp dir");
    let tool = support::sandbox::configured_shell_tool(dir.path(), Some(PIPEFAIL_SHELL));

    let defaulted = tool
        .run(
            params("false | true"),
            context(Arc::new(zuno_tool::NeverInterrupted)),
        )
        .await
        .expect("a non-zero exit is a completed call, not a tool failure");

    assert_eq!(
        defaulted.metadata["exit"], 1,
        "the default policy must not let a failing pipeline stage report success"
    );
    let evidence = receipt(&defaulted);
    assert_eq!(evidence.outcome, ReceiptOutcome::Failed);
    assert_eq!(evidence.exit_authority, ExitAuthority::Authoritative);
    assert!(!evidence.proves_success());

    let tolerated = tool
        .run(
            policy_params("false | true", ExitPolicy::Last),
            context(Arc::new(zuno_tool::NeverInterrupted)),
        )
        .await
        .expect("the opt-out still runs the command");

    assert_eq!(
        tolerated.metadata["exit"], 0,
        "`last` is the documented way to tolerate a failing stage"
    );
    let evidence = receipt(&tolerated);
    assert_eq!(evidence.outcome, ReceiptOutcome::Passed);
    assert_eq!(evidence.exit_authority, ExitAuthority::Derived);
    assert!(
        !evidence.proves_success(),
        "a status that covers only the last stage is not proof of success"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_failing_early_command_in_a_sequence_stops_the_run_only_under_the_all_policy() {
    let dir = tempfile::tempdir().expect("temp dir");
    let tool = support::sandbox::configured_shell_tool(dir.path(), Some(PIPEFAIL_SHELL));

    let stopped = tool
        .run(
            policy_params("false; echo continued", ExitPolicy::All),
            context(Arc::new(zuno_tool::NeverInterrupted)),
        )
        .await
        .expect("a failing sequence is a completed call");

    assert_eq!(stopped.metadata["exit"], 1);
    assert!(
        !stopped.output.contains("continued"),
        "`all` must stop at the first failing command: {}",
        stopped.output
    );

    let continued = tool
        .run(
            policy_params("false; echo continued", ExitPolicy::Pipefail),
            context(Arc::new(zuno_tool::NeverInterrupted)),
        )
        .await
        .expect("the default policy runs the whole sequence");

    assert_eq!(
        continued.metadata["exit"], 0,
        "`pipefail` covers pipelines, not sequences; that is what `all` adds"
    );
    assert!(continued.output.contains("continued"));
}

#[cfg(unix)]
#[tokio::test]
async fn a_passing_pipeline_reports_success_under_every_policy() {
    let dir = tempfile::tempdir().expect("temp dir");
    let tool = support::sandbox::configured_shell_tool(dir.path(), Some(PIPEFAIL_SHELL));

    for policy in [ExitPolicy::Pipefail, ExitPolicy::Last, ExitPolicy::All] {
        let output = tool
            .run(
                policy_params("printf 'a\\nb\\n' | grep -c b", policy),
                context(Arc::new(zuno_tool::NeverInterrupted)),
            )
            .await
            .expect("a genuinely passing pipeline");

        assert_eq!(output.metadata["exit"], 0, "{policy:?}");
        assert_eq!(output.output, "1\n", "{policy:?}");
        let evidence = receipt(&output);
        assert_eq!(evidence.outcome, ReceiptOutcome::Passed, "{policy:?}");
        assert_eq!(
            evidence.proves_success(),
            policy != ExitPolicy::Last,
            "only a policy whose status covers the whole command may prove success: {policy:?}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn only_a_real_pass_produces_a_receipt_that_proves_success() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().canonicalize().expect("canonical workspace");
    let tool = support::sandbox::configured_shell_tool(dir.path(), Some(PIPEFAIL_SHELL));

    let passed = tool
        .run(
            params("printf ok"),
            context(Arc::new(zuno_tool::NeverInterrupted)),
        )
        .await
        .expect("a passing command");
    let evidence = receipt(&passed);
    assert!(evidence.proves_success());
    assert_eq!(evidence.summary, "printf ok");
    assert_eq!(evidence.workdir, Some(workspace.display().to_string()));
    assert_eq!(evidence.exit_code, Some(0));
    assert_eq!(evidence.exit_authority, ExitAuthority::Authoritative);
    assert_eq!(
        evidence.output_digest,
        Some(hex::encode(Sha256::digest(b"ok"))),
        "the digest must cover the bytes the command actually produced"
    );
    assert_eq!(
        evidence.git_head, None,
        "nothing resolved HEAD for this call, so the receipt must not claim a revision"
    );
    assert_eq!(evidence.detail, None);

    let failed = tool
        .run(
            params("printf oops; exit 3"),
            context(Arc::new(zuno_tool::NeverInterrupted)),
        )
        .await
        .expect("a failing command still completes");
    let evidence = receipt(&failed);
    assert!(!evidence.proves_success());
    assert_eq!(evidence.outcome, ReceiptOutcome::Failed);
    assert_eq!(evidence.exit_code, Some(3));

    let promoted = tool
        .run(
            ShellParams {
                timeout: Some(40),
                ..params("sleep 1")
            },
            context(Arc::new(zuno_tool::NeverInterrupted)),
        )
        .await
        .expect("the foreground deadline promotes rather than fails");
    let evidence = receipt(&promoted);
    assert!(
        !evidence.proves_success(),
        "a command that has not finished proves nothing"
    );
    assert_eq!(evidence.outcome, ReceiptOutcome::Unknown);
    assert_eq!(evidence.exit_authority, ExitAuthority::Absent);
    assert_eq!(evidence.exit_code, None);
    assert!(
        evidence
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("foreground")),
        "{:?}",
        evidence.detail
    );

    let launched = tool
        .run(
            ShellParams {
                background: true,
                ..params("printf started")
            },
            context(Arc::new(zuno_tool::NeverInterrupted)),
        )
        .await
        .expect("a background launch");
    let evidence = receipt(&launched);
    assert!(
        !evidence.proves_success(),
        "a launch is not an outcome, however fast the command is"
    );
    assert_eq!(evidence.outcome, ReceiptOutcome::Unknown);
    assert_eq!(evidence.exit_authority, ExitAuthority::Absent);
    assert_eq!(evidence.output_digest, None);
}

#[cfg(unix)]
#[tokio::test]
async fn the_receipt_reaches_the_host_under_the_documented_metadata_key() {
    let dir = tempfile::tempdir().expect("temp dir");
    let tool = support::sandbox::configured_shell_tool(dir.path(), Some(PIPEFAIL_SHELL));

    let output = tool
        .run(
            params("printf ok"),
            context(Arc::new(zuno_tool::NeverInterrupted)),
        )
        .await
        .expect("a passing command");

    assert_eq!(
        output.metadata["exit"], 0,
        "the pre-existing key clients already read must not move"
    );
    assert_eq!(
        output.metadata[VERIFICATION_METADATA_KEY]["exitAuthority"],
        "authoritative"
    );
    let decoded = VerificationReceipt::from_metadata(&output.metadata)
        .expect("a receipt the host cannot decode is worse than none")
        .expect("a completed shell result carries one");
    assert!(decoded.proves_success());
}

#[test]
fn shell_description_says_a_pipeline_exit_code_needs_pipefail() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let tool = support::sandbox::shell_tool(workspace.path());
    let description = tool.description();

    for clause in [
        "A pipeline's exit code is meaningless unless pipefail is in effect",
        "`exitPolicy` defaults to `\"pipefail\"`",
        "`exitPolicy: \"last\"` is the way to run a command that is meant to tolerate a failing \
         stage",
        "PowerShell has no pipefail equivalent",
    ] {
        assert!(
            description.contains(clause),
            "shell description is missing `{clause}`:\n{description}"
        );
    }
}

#[test]
fn shell_schema_publishes_the_three_exit_policies_without_requiring_one() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let tool = support::sandbox::shell_tool(workspace.path());
    let definition = tool.definition();
    let policy = definition.parameters["properties"]
        .get("exitPolicy")
        .expect("Shell omitted the exitPolicy field")
        .to_string();

    for value in ["pipefail", "last", "all"] {
        assert!(policy.contains(value), "{policy}");
    }
    assert!(
        !definition.parameters["required"]
            .as_array()
            .is_some_and(|required| required.iter().any(|name| name == "exitPolicy")),
        "exitPolicy must stay optional: {}",
        definition.parameters
    );
}

#[cfg(unix)]
async fn wait_for_file(path: &Path) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{} was not created", path.display()));
}

#[cfg(unix)]
fn read_pid(path: &Path) -> u32 {
    std::fs::read_to_string(path)
        .expect("pid file")
        .parse()
        .expect("numeric pid")
}

#[cfg(unix)]
async fn wait_for_process_exit(pid: u32) {
    let proc_path = std::path::PathBuf::from(format!("/proc/{pid}"));
    tokio::time::timeout(Duration::from_secs(1), async {
        while proc_path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("process {pid} survived group termination"));
}

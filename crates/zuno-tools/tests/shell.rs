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
use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt as _;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::process::Command;
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// Puts a fake `git` where the tool will find it first.
///
/// The pre-flight reads and the command itself both resolve `git` through the environment
/// the tool hands the child, so replacing `PATH` is what lets a test decide how long one
/// of those reads takes.
#[cfg(unix)]
struct OnlyFakeGitOnPath {
    bin: String,
}

#[cfg(unix)]
#[async_trait]
impl ShellEnvHook for OnlyFakeGitOnPath {
    async fn env(&self, _input: ShellEnvInput) -> Result<BTreeMap<String, String>, ToolError> {
        Ok(BTreeMap::from([("PATH".to_owned(), self.bin.clone())]))
    }
}

#[cfg(unix)]
fn write_fake_git(bin: &Path, body: &str) {
    let path = bin.join("git");
    // The child's `PATH` holds this directory alone, so the script names its own search
    // path instead of inheriting one it cannot rely on.
    std::fs::write(&path, format!("#!/bin/sh\nPATH=/usr/bin:/bin\n{body}"))
        .expect("write the fake git");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("make the fake git executable");
}

/// A pre-flight `git` read leaves the runtime free to poll everything else.
///
/// `zuno serve`, `zuno acp`, and `zuno run` all drive a current-thread runtime — the
/// flavour this test runs on — so waiting on git synchronously parked every session in
/// the process: no provider stream advanced, no SSE frame was written and no interrupt
/// was observed for as long as the untracked-file walk took.
#[cfg(unix)]
#[tokio::test]
async fn a_pre_flight_git_read_leaves_the_runtime_polling() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let bin = tempfile::tempdir().expect("fake git directory");
    write_fake_git(
        bin.path(),
        "if [ \"$1\" = rev-parse ]; then sleep 2; exit 1; fi\nexit 0\n",
    );
    let tool =
        support::sandbox::shell_tool(workspace.path()).with_env_hook(Arc::new(OnlyFakeGitOnPath {
            bin: bin.path().display().to_string(),
        }));

    let started = Instant::now();
    let woke_after = Arc::new(AtomicU64::new(0));
    let elapsed = Arc::clone(&woke_after);
    let timer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        elapsed.store(
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            Ordering::SeqCst,
        );
    });

    tool.run(
        params("git commit --quiet -m wip"),
        context(Arc::new(NeverInterrupted)),
    )
    .await
    .expect("the fake git reports no repository, so the commit is not the tool's to refuse");
    timer.await.expect("the timer task joins");

    let woke_after = woke_after.load(Ordering::SeqCst);
    assert!(
        woke_after < 1_000,
        "a 100ms timer resolved after {woke_after}ms: the two seconds git spent answering \
         `rev-parse` were two seconds this runtime polled nothing else"
    );
}

/// A pre-flight `git` read that never answers refuses the call instead of allowing it.
///
/// These reads decide whether a commit delivers Zuno's own generated state, so a read
/// that outlives its ceiling leaves the repository state unknown, not empty. Reading it
/// as "nothing staged" would turn a `.git` on a stalled mount into permission to commit
/// whatever is there, which is the one outcome a timeout must not buy.
#[cfg(unix)]
#[tokio::test]
async fn a_pre_flight_git_read_that_never_answers_refuses_the_commit() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let bin = tempfile::tempdir().expect("fake git directory");
    let pid_file = workspace.path().join("git.pid");
    // `exec` so the recorded pid is the process that outlives the ceiling, and the
    // teardown assertion below is about that process rather than a shell that wrapped it.
    write_fake_git(
        bin.path(),
        &format!(
            "printf '%s' \"$$\" > '{}'\nexec sleep 30\n",
            pid_file.display()
        ),
    );
    let tool = support::sandbox::shell_tool(workspace.path())
        .with_env_hook(Arc::new(OnlyFakeGitOnPath {
            bin: bin.path().display().to_string(),
        }))
        .with_git_ceiling(Duration::from_millis(200));

    let error = tokio::time::timeout(
        Duration::from_secs(5),
        tool.run(
            params("git commit --quiet -m wip"),
            context(Arc::new(NeverInterrupted)),
        ),
    )
    .await
    .expect("an unanswered pre-flight read must settle at its ceiling, not hang the call")
    .expect_err("an unknown repository state must not admit the commit");
    let rendered = format!("{error:?}");
    assert!(
        rendered.contains("did not answer within the 200ms"),
        "{rendered}"
    );
    assert!(rendered.contains("it was not run"), "{rendered}");

    wait_for_process_exit(read_pid(&pid_file)).await;
}

/// A user's interrupt ends a hung pre-flight `git` read instead of waiting out the ceiling.
///
/// The ceiling made the reads survivable for the rest of the process; it did nothing for
/// the user in front of this call, who had no way to stop it — and putting the read in its
/// own process group took away the terminal `SIGINT` that used to reach a hung pre-flight
/// git under `zuno run`. `ctx.interrupt` is the cancellation every client surface has, so
/// it is the one the reads have to answer.
#[cfg(unix)]
#[tokio::test]
async fn an_interrupt_ends_a_hung_pre_flight_git_read() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let bin = tempfile::tempdir().expect("fake git directory");
    let pid_file = workspace.path().join("git.pid");
    write_fake_git(
        bin.path(),
        &format!(
            "printf '%s' \"$$\" > '{}'\nexec sleep 60\n",
            pid_file.display()
        ),
    );
    let tool = support::sandbox::shell_tool(workspace.path())
        .with_env_hook(Arc::new(OnlyFakeGitOnPath {
            bin: bin.path().display().to_string(),
        }))
        // Far beyond the assertion window, so only the interrupt can end this call.
        .with_git_ceiling(Duration::from_secs(600));
    let interrupt = Arc::new(FirableInterrupt::default());
    let fires = Arc::clone(&interrupt);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        fires.fire();
    });

    let started = Instant::now();
    let error = tokio::time::timeout(
        Duration::from_secs(10),
        tool.run(params("git commit --quiet -m wip"), context(interrupt)),
    )
    .await
    .expect(
        "an interrupted pre-flight read must settle when the user asks, not when its \
         ten-minute ceiling expires",
    )
    .expect_err("an interrupted call has no repository answer to report");

    let rendered = format!("{error:?}");
    assert!(
        rendered.contains("shell command was interrupted"),
        "{rendered}"
    );
    let waited = started.elapsed();
    assert!(
        waited < Duration::from_secs(5),
        "the call took {waited:?} to answer an interrupt fired after 300ms"
    );
    // Abandoning the read is not enough: the group it leads has to go with it, or a
    // credential helper git spawned outlives the cancellation with nothing left to reap it.
    wait_for_process_exit(read_pid(&pid_file)).await;
}

/// The pre-flight ceiling covers the whole phase, not each read on its own.
///
/// `refuse_generated_delivery` makes up to four reads and `validate_expected_git_head` one
/// more. Given a ceiling each, a `.git` that answers slowly but does answer left the call
/// unresponsive for five times the number the ceiling advertises — with the 30s default,
/// about two and a half minutes.
#[cfg(unix)]
#[tokio::test]
async fn the_pre_flight_ceiling_covers_the_phase_rather_than_each_read() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let bin = tempfile::tempdir().expect("fake git directory");
    // Every read answers, and answers well inside a 500ms per-read ceiling. Only a shared
    // deadline can notice that four of them do not fit inside one.
    write_fake_git(
        bin.path(),
        &format!(
            "sleep 0.2\nif [ \"$1\" = rev-parse ]; then printf '%s' '{}'; fi\nexit 0\n",
            workspace.path().display()
        ),
    );
    let tool = support::sandbox::shell_tool(workspace.path())
        .with_env_hook(Arc::new(OnlyFakeGitOnPath {
            bin: bin.path().display().to_string(),
        }))
        .with_git_ceiling(Duration::from_millis(500));

    let error = tokio::time::timeout(
        Duration::from_secs(10),
        tool.run(
            params("git add -A && git commit --quiet -m wip"),
            context(Arc::new(NeverInterrupted)),
        ),
    )
    .await
    .expect("the phase must settle")
    .expect_err("four 200ms reads do not fit in a 500ms phase, so the state is unknown");

    let rendered = format!("{error:?}");
    assert!(
        rendered.contains("did not answer within the 500ms"),
        "{rendered}"
    );
    assert!(rendered.contains("it was not run"), "{rendered}");
}

/// A byte no UTF-8 sequence can start with, which is what makes an entry unspellable.
///
/// `0xE9` is `é` in Latin-1, so this is not a synthetic hostile input: it is what a
/// `LANG=en_US.ISO-8859-1` login, a Windows console codepage, or a filename from a
/// pre-Unicode archive puts in the environment of the process that launches Zuno.
#[cfg(unix)]
const LATIN1_E_ACUTE: u8 = 0xE9;

/// An environment variable value that is not Unicode.
#[cfg(unix)]
fn non_unicode_value() -> OsString {
    let mut bytes = b"caf".to_vec();
    bytes.push(LATIN1_E_ACUTE);
    OsString::from_vec(bytes)
}

/// An environment variable *name* that is not Unicode.
///
/// Set in the child so `std::env::vars` has something to panic on, and deliberately not
/// followed into the command: `/bin/sh` drops an environment name that is not a valid shell
/// identifier before `env` can print it (measured: absent through `sh -c env`, present
/// through a direct `exec` of `env`), so no command run through a shell can witness it.
/// What Zuno does with it is pinned on [`zuno_tools::shell`]'s own withholding decision at
/// unit level.
#[cfg(unix)]
fn non_unicode_name() -> OsString {
    let mut bytes = b"LOCALE_NAME_PROBE_".to_vec();
    bytes.push(LATIN1_E_ACUTE);
    OsString::from_vec(bytes)
}

/// This process's environment entries that cannot be spelled as UTF-8.
///
/// Asserted before anything else in the child: on a platform or a libc that refused to pass
/// the bytes through, every assertion about them would hold vacuously.
#[cfg(unix)]
fn unspellable_environment_entries() -> Vec<(OsString, OsString)> {
    std::env::vars_os()
        .filter(|(name, value)| name.to_str().is_none() || value.to_str().is_none())
        .collect()
}

/// The `#[test]` this test re-execs itself as.
///
/// A `--exact` filter that matches nothing is not an error to libtest: it prints
/// `ok. 0 passed; 0 failed; 35 filtered out` and exits 0, so a parent that asserts only
/// `status.success()` passes without a single assertion having run. Renaming the test
/// below is therefore enough to disarm it silently. Two things stop that here: the parent
/// asserts this name appears in the binary's own `--list`, and it insists on seeing
/// [`WITHHOLDING_OBSERVED`], which only the child's last line can print.
#[cfg(unix)]
const WITHHOLDING_TEST: &str =
    "zunos_own_environment_is_absent_from_a_composed_commands_environment";

/// The child's proof that it ran to the end of its assertions.
#[cfg(unix)]
const WITHHOLDING_OBSERVED: &str = "zuno-tools: the composed command's environment was observed";

/// Zuno's own environment is not part of the environment a model-composed command reads.
///
/// The assertions run in a child of this test binary because the variables have to be
/// real: only a process that actually carries `ZUNO_CONFIG_CONTENT` shows what a `shell`
/// call can read, and setting one in this process is `unsafe`, which this workspace
/// forbids.
#[cfg(unix)]
#[tokio::test]
async fn zunos_own_environment_is_absent_from_a_composed_commands_environment() {
    const CHILD: &str = "ZUNO_TOOLS_WITHHOLDING_CHILD";
    const LEAKED: &str = "sk-live-must-not-leak";

    if std::env::var_os(CHILD).is_none() {
        let binary = std::env::current_exe().expect("this test binary");
        // Before the run that matters: a filter naming no test is a silent pass, so the
        // name is checked against the binary's own catalogue first.
        let listed = Command::new(&binary)
            .args(["--list", "--format", "terse"])
            .output()
            .expect("list this binary's tests");
        let listed = String::from_utf8_lossy(&listed.stdout).into_owned();
        assert!(
            listed.contains(&format!("{WITHHOLDING_TEST}: test")),
            "`{WITHHOLDING_TEST}` is not a test in this binary, so the re-exec below would \
             filter to nothing and pass without asserting anything:\n{listed}"
        );

        let child = Command::new(&binary)
            .args(["--exact", WITHHOLDING_TEST, "--nocapture"])
            .env(CHILD, "1")
            .env(
                "ZUNO_CONFIG_CONTENT",
                format!(r#"{{"provider":{{"anthropic":{{"options":{{"apiKey":"{LEAKED}"}}}}}}}}"#),
            )
            .env(
                "ZUNO_AUTH_CONTENT",
                format!(r#"{{"anthropic":{{"key":"{LEAKED}"}}}}"#),
            )
            // Named nowhere in the tool: whatever Zuno reads next is withheld without
            // anyone having had to anticipate that it holds a secret.
            .env("ZUNO_PROVIDER_TOKEN", LEAKED)
            .env("ZUNO_WORKSPACE_ID", "wsp_1")
            .env("ZUNO_PID", std::process::id().to_string())
            // The bare marker, so the child can pin that it survived the namespace rule.
            .env("ZUNO", "1")
            // An entry Zuno cannot spell, in both polarities. `std::env::vars` *panics* on
            // a name or value that is not Unicode, and the panic unwound the turn rather
            // than the call: in a process launched with one Latin-1 byte anywhere in its
            // environment, every `shell` call failed and no message named why.
            .env("LOCALE_PROBE", non_unicode_value())
            .env("ZUNO_PROBE", non_unicode_value())
            .env(non_unicode_name(), non_unicode_value())
            .output()
            .expect("re-run this test with Zuno's own environment set");
        let stdout = String::from_utf8_lossy(&child.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&child.stderr).into_owned();
        assert!(
            child.status.success(),
            "the child's assertions must hold:\n{stdout}\n{stderr}"
        );
        assert!(
            stdout.contains(WITHHOLDING_OBSERVED),
            "the child never reported observing a composed command's environment, so \
             nothing was asserted:\n{stdout}\n{stderr}"
        );
        return;
    }

    let workspace = tempfile::tempdir().expect("temporary workspace");
    let tool = support::sandbox::shell_tool(workspace.path());

    // The exfiltration this closes is one `env` away from being a network request.
    let output = tool
        .run(params("env"), context(Arc::new(NeverInterrupted)))
        .await
        .expect("reading the environment succeeds");

    assert!(!output.output.contains(LEAKED), "{}", output.output);
    // Identity is withheld too. `ZUNO_PID` is the address of every value above: on this
    // unconfined path `tr '\0' '\n' < /proc/$ZUNO_PID/environ` reads them back out of the
    // Zuno process, so handing the pid to the command is handing over the documents.
    for withheld in ["ZUNO_PID=", "ZUNO_WORKSPACE_ID=", "ZUNO_CONFIG_CONTENT="] {
        assert!(
            !output.output.contains(withheld),
            "{withheld} reached the command:\n{}",
            output.output
        );
    }
    // The bare markers are outside the namespace, so "am I under Zuno?" still answers.
    assert!(
        output.output.lines().any(|line| line == "ZUNO=1"),
        "{}",
        output.output
    );

    // The entries that cannot be spelled. Read back as bytes rather than out of
    // `output.output`, which is a `String` and would have replaced them with U+FFFD before
    // the assertion could see them.
    let unspellable = unspellable_environment_entries();
    assert!(
        unspellable.len() >= 3,
        "this process carries {} environment entries that are not Unicode, not the three the \
         parent set, so every assertion below would hold without the hostile case existing",
        unspellable.len()
    );
    assert!(
        unspellable
            .iter()
            .any(|(name, _)| name.as_encoded_bytes().starts_with(b"LOCALE_NAME_PROBE_")),
        "the entry whose *name* is not Unicode did not survive the spawn, so the panic this \
         covers would not be reachable from here"
    );
    tool.run(params("env > raw.env"), context(Arc::new(NeverInterrupted)))
        .await
        .expect("writing the environment out succeeds");
    let raw = std::fs::read(workspace.path().join("raw.env")).expect("read the environment back");
    let mut preserved = b"LOCALE_PROBE=".to_vec();
    preserved.extend_from_slice(&non_unicode_value().into_vec());
    assert!(
        raw.windows(preserved.len())
            .any(|window| window == preserved),
        "the entry Zuno cannot spell did not reach the command unchanged. Dropping it would \
         silently change the environment the command runs in; preserving it is the decision \
         this pins:\n{}",
        String::from_utf8_lossy(&raw)
    );
    assert!(
        !raw.windows(b"ZUNO_PROBE".len())
            .any(|window| window == b"ZUNO_PROBE"),
        "a withheld name reached the command because its value was not Unicode:\n{}",
        String::from_utf8_lossy(&raw)
    );
    println!("{WITHHOLDING_OBSERVED}");
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
    let output = tokio::time::timeout(Duration::from_secs(2), running)
        .await
        .expect("cancellation must finish promptly")
        .expect("task join")
        .expect("a cancelled command settles rather than failing");

    // Killing the tree is the behaviour under test; the result shape is asserted here
    // only so that a future change cannot go back to reporting the kill as a clean,
    // certain cancellation.
    assert_eq!(output.metadata["cancellation"]["uncertain"], true);
    wait_for_process_exit(parent).await;
    wait_for_process_exit(child).await;
}

/// Cancelling a running command hands back what it had already written.
///
/// Everything the command produced used to be discarded on the way out, and the model
/// was told the tool had completed its cleanup — a clean, certain, effect-free reading
/// of a command that was killed mid-flight. The bytes, the absent exit status, and the
/// demand for state inspection all have to survive.
#[cfg(unix)]
#[tokio::test]
async fn shell_cancellation_returns_the_bytes_the_command_had_already_written() {
    let dir = tempfile::tempdir().expect("temp dir");
    let marker = dir.path().join("printed");
    // An external `echo` rather than the shell's builtin: an exited process has flushed
    // its bytes into the pipe, so what the test waits for is what the capture holds.
    let command = format!(
        "/bin/echo 'partial progress'; : > '{}'; sleep 30",
        marker.display()
    );
    let interrupt = Arc::new(FirableInterrupt::default());
    let tool = support::sandbox::shell_tool(dir.path());
    let run_context = context(interrupt.clone());

    let running = tokio::spawn(async move { tool.run(params(command), run_context).await });
    wait_for_file(&marker).await;
    interrupt.fire();
    let output = tokio::time::timeout(Duration::from_secs(2), running)
        .await
        .expect("cancellation must finish promptly")
        .expect("task join")
        .expect("a cancelled command is a settled result, not a failure");

    assert!(
        output.output.contains("partial progress"),
        "cancellation must preserve captured output: {}",
        output.output
    );
    assert!(
        output.output.contains("Inspect the authoritative state"),
        "a killed command's outcome is not certain: {}",
        output.output
    );
    assert_eq!(output.metadata["cancellation"]["cancelled"], true);
    assert_eq!(output.metadata["cancellation"]["uncertain"], true);
    assert_eq!(output.metadata["cancellation"]["authoritative"], false);
    assert_eq!(output.metadata["exit"], serde_json::Value::Null);

    let receipt = receipt(&output);
    assert!(!receipt.proves_success());
    assert_eq!(receipt.outcome, ReceiptOutcome::Unknown);
    assert_eq!(receipt.exit_authority, ExitAuthority::Absent);
    assert_eq!(receipt.exit_code, None);
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
    let window = store
        .read_window(
            "shell",
            &context(Arc::new(zuno_tool::NeverInterrupted)).session_id,
            std::path::Path::new(path),
            0,
            4_096,
        )
        .expect("stored full output");
    assert_eq!(
        String::from_utf8(window.bytes).expect("text"),
        output.output
    );
    assert_eq!(window.cursor, window.total);
}

/// A generated root the caller already resolved is the one the tool writes under.
///
/// This entry point exists so an async caller can resolve the root off-reactor:
/// `zuno_paths::generated_root` spawns up to three synchronous `git rev-parse` calls, each
/// bounded at ten seconds inside `zuno_paths`, and a current-thread runtime — `zuno run`,
/// `zuno acp`, `zuno serve` — has no other thread to run them on, so constructing the tool
/// stalled every session in the process for up to thirty seconds against a `.git` on a
/// stalled mount. Handing the answer in is only worth anything if the answer is used, so
/// what is pinned here is the observable consequence: the artefact of an oversized command
/// lands under the supplied root, and under the default it lands under the workspace, which
/// is what would happen if the parameter were quietly ignored.
#[cfg(unix)]
#[tokio::test]
async fn an_explicitly_resolved_generated_root_is_where_the_shell_tool_saves_output() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let elsewhere = tempfile::tempdir().expect("generated root");
    let limits = OutputLimits {
        max_lines: 1,
        max_bytes: 4,
    };

    let injected =
        support::sandbox::shell_tool_with_generated_root(workspace.path(), elsewhere.path())
            .with_output_limits(limits);
    let output = injected
        .execute(
            json!({
                "command": "printf 'one\\ntwo\\n'",
                ACCEPT_LARGE_OUTPUT_KEY: true,
            }),
            context(Arc::new(NeverInterrupted)),
        )
        .await
        .expect("explicitly accepted command output succeeds");
    let paths = output.output_paths();
    let saved = std::path::Path::new(paths.first().expect("stored output path"));
    let expected = elsewhere.path().join(".zuno").join("tool-output");
    assert!(
        saved.starts_with(&expected),
        "{} is not under the generated root that was supplied ({})",
        saved.display(),
        expected.display()
    );

    // Unset, the tool resolves the root itself, which for a workspace that is not a
    // repository is the workspace: the parameter changed where output went.
    let default = support::sandbox::shell_tool(workspace.path()).with_output_limits(limits);
    let output = default
        .execute(
            json!({
                "command": "printf 'one\\ntwo\\n'",
                ACCEPT_LARGE_OUTPUT_KEY: true,
            }),
            context(Arc::new(NeverInterrupted)),
        )
        .await
        .expect("explicitly accepted command output succeeds");
    let paths = output.output_paths();
    let saved = std::path::Path::new(paths.first().expect("stored output path"));
    assert!(
        !saved.starts_with(elsewhere.path()),
        "{} landed under the injected root without anyone asking",
        saved.display()
    );
}

/// A command that succeeded and produced too much output is still a successful result.
///
/// Withholding used to be returned as a tool failure, which threw away the exit code,
/// the verification receipt, and the artifact reference of a command that had run
/// perfectly, and left the model with prose telling it to re-run a call `shell` declares
/// must never be replayed.
#[cfg(unix)]
#[tokio::test]
async fn shell_withheld_output_keeps_the_receipt_and_offers_the_windowed_read() {
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
            json!({ "command": "printf 'one\\ntwo\\n'" }),
            context(Arc::new(zuno_tool::NeverInterrupted)),
        )
        .await
        .expect("a command that ran does not fail because its output was large");

    assert_eq!(output.metadata["exit"], 0);
    assert_eq!(
        output.metadata[VERIFICATION_METADATA_KEY]["exitAuthority"],
        "authoritative"
    );
    assert_eq!(output.metadata["oversized"], true);
    assert!(
        output.output.contains("Tool output withheld"),
        "{}",
        output.output
    );
    assert!(output.output.contains("`bg`"), "{}", output.output);

    let paths = output.output_paths();
    let path = paths.first().expect("stored output path");
    let window = store
        .read_window(
            "shell",
            &context(Arc::new(zuno_tool::NeverInterrupted)).session_id,
            std::path::Path::new(path),
            0,
            4_096,
        )
        .expect("stored full output");
    assert_eq!(String::from_utf8(window.bytes).expect("text"), "one\ntwo\n");
}

/// The artifact holds the command's own bytes, even when they are not text.
///
/// The foreground handoff removes the execution's `.output` file as it hands the bytes
/// back, so the artifact written here is the only copy that outlives the call. Decoding
/// before persisting made that copy a record of the damage instead of the output.
#[cfg(unix)]
#[tokio::test]
async fn shell_persists_the_bytes_a_command_wrote_not_their_lossy_decoding() {
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
            json!({ "command": r"printf 'a\nb\377\376\n'" }),
            context(Arc::new(zuno_tool::NeverInterrupted)),
        )
        .await
        .expect("withheld");

    let paths = output.output_paths();
    let path = paths.first().expect("stored output path");
    let window = store
        .read_window(
            "shell",
            &context(Arc::new(zuno_tool::NeverInterrupted)).session_id,
            std::path::Path::new(path),
            0,
            4_096,
        )
        .expect("stored full output");
    assert!(
        window.bytes.contains(&0xff) && window.bytes.contains(&0xfe),
        "the artifact must keep the bytes, not U+FFFD: {:?}",
        window.bytes
    );
    assert!(
        !output.output.contains('\u{fffd}'),
        "the notice replaces the decoded text entirely: {}",
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

/// Whether `pid` still names a process, reaped or not.
///
/// `ps -p` rather than `/proc/{pid}`: `/proc` is Linux, so a `cfg(unix)` helper that probes
/// it makes every macOS run of the assertions below vacuously true — the path never exists,
/// so the first check passes without having observed anything.
#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    Command::new("ps")
        .args(["-p", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The probe itself has to be able to see a live process.
///
/// `ps` missing, or refusing, would make [`process_exists`] answer `false` for every pid,
/// and every "it is gone" assertion below would pass without having observed anything —
/// the same vacuous shape as probing `/proc` on macOS. This process's own pid is the one
/// case whose answer is known.
#[cfg(unix)]
fn assert_process_probe_works() {
    assert!(
        process_exists(std::process::id()),
        "`ps -p` cannot see this test process, so it cannot witness any other process \
         either: the exit assertions would be vacuous"
    );
}

#[cfg(unix)]
async fn wait_for_process_exit(pid: u32) {
    assert_process_probe_works();
    tokio::time::timeout(Duration::from_secs(2), async {
        while process_exists(pid) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("process {pid} survived group termination"));
}

/// The exact input the goal store recorded as unreachable: a workspace edit made by
/// running a command.
///
/// `crates/zuno-goal/src/store_tests.rs` names it verbatim —
/// `shell {"command": "sed -i 's/foo/bar/' crates/zuno-parser/src/lib.rs"}` — and its
/// premise is that the call reports no written path, so the store is told nothing happened
/// and a user-created goal completes with zero evidence. This is the report that premise
/// waited for.
#[cfg(unix)]
#[tokio::test]
async fn a_command_that_edits_a_file_reports_the_path_it_wrote() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let source = workspace.path().join("crates/zuno-parser/src");
    std::fs::create_dir_all(&source).expect("create the source tree");
    let subject = source.join("lib.rs");
    std::fs::write(&subject, "fn foo() {}\n").expect("write the file the command edits");
    let tool = support::sandbox::shell_tool(workspace.path());

    let output = tool
        .run(
            params("sed -i 's/foo/bar/' crates/zuno-parser/src/lib.rs"),
            context(Arc::new(NeverInterrupted)),
        )
        .await
        .expect("the edit succeeds");

    let reported = output.written_paths();
    let canonical = subject.canonicalize().expect("canonical subject");
    assert_eq!(
        reported,
        vec![zuno_paths::wire_path(&canonical)],
        "the command edited {} and reported {reported:?}",
        canonical.display()
    );
    assert_eq!(
        std::fs::read_to_string(&subject).expect("read back"),
        "fn bar() {}\n",
        "the command really did rewrite the file"
    );
}

/// A command that only reads reports nothing, so the report cannot escalate a goal that
/// changed nothing.
///
/// The report is two `stat`s that disagree, not a guess from the command line: `cat` and
/// `grep` name the same file the `sed` above named, and neither is reported.
#[cfg(unix)]
#[tokio::test]
async fn a_command_that_only_reads_reports_no_written_path() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let subject = workspace.path().join("lib.rs");
    std::fs::write(&subject, "fn foo() {}\n").expect("write the file the command reads");
    let tool = support::sandbox::shell_tool(workspace.path());

    for command in ["cat lib.rs", "grep foo lib.rs", "wc -l lib.rs"] {
        let output = tool
            .run(params(command), context(Arc::new(NeverInterrupted)))
            .await
            .expect("the read succeeds");
        assert!(
            output.written_paths().is_empty(),
            "`{command}` reported a write: {:?}",
            output.written_paths()
        );
    }
}

/// A created file is reported and a deleted one is not.
///
/// [`zuno_tool::METADATA_WRITTEN_PATHS_KEY`] means "this file is now here to be re-read",
/// so a path the command removed is deliberately absent: a consumer that re-read it would
/// find nothing, and the tools that already report writes report deletions nowhere either.
#[cfg(unix)]
#[tokio::test]
async fn a_created_file_is_reported_and_a_removed_one_is_not() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let doomed = workspace.path().join("doomed.txt");
    std::fs::write(&doomed, "gone soon\n").expect("write the file the command removes");
    let tool = support::sandbox::shell_tool(workspace.path());

    let created = tool
        .run(
            params("touch fresh.txt"),
            context(Arc::new(NeverInterrupted)),
        )
        .await
        .expect("the create succeeds");
    let fresh = workspace
        .path()
        .join("fresh.txt")
        .canonicalize()
        .expect("canonical fresh path");
    assert_eq!(created.written_paths(), vec![zuno_paths::wire_path(&fresh)]);

    let removed = tool
        .run(params("rm doomed.txt"), context(Arc::new(NeverInterrupted)))
        .await
        .expect("the removal succeeds");
    assert!(
        removed.written_paths().is_empty(),
        "a removed path was reported as written: {:?}",
        removed.written_paths()
    );
    assert!(!doomed.exists(), "the command really did remove the file");
}

/// A target the shell would expand is not reported, and the boundary is stated rather than
/// guessed at.
///
/// Resolving `*.rs` or `$OUT` here would mean re-implementing the shell's own expansion,
/// and a report that names a path the command never touched is worse than a short one: a
/// consumer re-reads it and retires evidence over it. So the report is a lower bound, and
/// this pins where the bound is — the same command with the name written out is reported.
#[cfg(unix)]
#[tokio::test]
async fn a_target_the_shell_expands_is_left_out_of_the_report() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let subject = workspace.path().join("lib.rs");
    std::fs::write(&subject, "fn foo() {}\n").expect("write the file the command edits");
    let tool = support::sandbox::shell_tool(workspace.path());

    let expanded = tool
        .run(
            params("sed -i 's/foo/bar/' *.rs"),
            context(Arc::new(NeverInterrupted)),
        )
        .await
        .expect("the edit succeeds");

    assert_eq!(
        std::fs::read_to_string(&subject).expect("read back"),
        "fn bar() {}\n",
        "the glob really did reach the file"
    );
    assert!(
        expanded.written_paths().is_empty(),
        "a glob was resolved into a reported path: {:?}",
        expanded.written_paths()
    );

    // The same write, named statically, is reported: the gap is expansion, not the tool.
    let named = tool
        .run(
            params("sed -i 's/bar/baz/' lib.rs"),
            context(Arc::new(NeverInterrupted)),
        )
        .await
        .expect("the edit succeeds");
    assert_eq!(
        named.written_paths(),
        vec![zuno_paths::wire_path(
            &subject.canonicalize().expect("canonical subject")
        )]
    );
}

/// A command that wrote and then failed still reports what it wrote.
///
/// The exit status decides whether the *command* succeeded; it says nothing about whether
/// the workspace changed. A consumer told nothing here would go on citing a verification
/// receipt that no longer describes the file.
#[cfg(unix)]
#[tokio::test]
async fn a_command_that_wrote_before_failing_still_reports_the_write() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let subject = workspace.path().join("half-done.txt");
    let tool = support::sandbox::shell_tool(workspace.path());

    let output = tool
        .run(
            params("touch half-done.txt && cat missing.txt"),
            context(Arc::new(NeverInterrupted)),
        )
        .await
        .expect("a non-zero exit is a result, not an error");

    assert_eq!(
        output.written_paths(),
        vec![zuno_paths::wire_path(
            &subject.canonicalize().expect("canonical subject")
        )],
        "the file the command created before failing was not reported"
    );
}

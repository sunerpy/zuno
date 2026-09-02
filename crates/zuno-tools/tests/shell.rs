mod support;

#[cfg(unix)]
use async_trait::async_trait;
#[cfg(unix)]
use serde_json::json;
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
use zuno_tool::{OutputLimits, ToolOutputStore};
#[cfg(unix)]
use zuno_tools::shell::{ShellEnvHook, ShellEnvInput, ShellParams};
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
    }
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

use async_trait::async_trait;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use zuno_error::ToolError;
use zuno_permission::{PermissionAction, Rule, evaluate};
use zuno_tool::{ACCEPT_LARGE_OUTPUT_KEY, AllowAll, InterruptHandle, Tool, ToolContext};
use zuno_tool::{OutputLimits, ToolOutputStore};
use zuno_tools::shell::{
    ShellEnvHook, ShellEnvInput, ShellParams, ShellSyntax, ShellTool, analyze_command,
};

#[derive(Default)]
struct FirableInterrupt {
    fired: AtomicBool,
    notify: Notify,
}

impl FirableInterrupt {
    fn fire(&self) {
        self.fired.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

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

fn params(command: impl Into<String>) -> ShellParams {
    ShellParams {
        command: command.into(),
        timeout: None,
        workdir: None,
        background: false,
        justification: None,
    }
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
            permission: "bash".to_owned(),
            pattern: "*".to_owned(),
            action: PermissionAction::Deny,
        },
        Rule {
            permission: "bash".to_owned(),
            pattern: "git push*".to_owned(),
            action: PermissionAction::Ask,
        },
    ];
    assert_eq!(
        evaluate("bash", &analysis.commands[1].source, &rules),
        PermissionAction::Ask,
        "the extracted constituent, not the opaque compound string, must reach zuno-permission"
    );
    assert_eq!(
        evaluate("bash", &analysis.commands[0].source, &rules),
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
    let tool = ShellTool::new(dir.path()).expect("shell tool");
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
async fn shell_injected_hard_ceiling_really_terminates_the_process_under_two_seconds() {
    let dir = tempfile::tempdir().expect("temp dir");
    let pid_file = dir.path().join("ceiling.pid");
    let command = format!("printf '%s' \"$$\" > '{}'; sleep 30", pid_file.display());
    let tool = ShellTool::new(dir.path())
        .expect("shell tool")
        .with_hard_ceiling(Duration::from_millis(100));
    let started = Instant::now();

    let error = tool
        .run(
            params(command),
            context(Arc::new(zuno_tool::NeverInterrupted)),
        )
        .await
        .expect_err("the hard ceiling must fail the call");

    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(matches!(error, ToolError::Timeout { .. }));
    wait_for_file(&pid_file).await;
    wait_for_process_exit(read_pid(&pid_file)).await;
}

struct InjectEnv;

#[async_trait]
impl ShellEnvHook for InjectEnv {
    async fn env(&self, input: ShellEnvInput) -> Result<BTreeMap<String, String>, ToolError> {
        assert_eq!(input.session_id, "ses_shell");
        assert_eq!(input.call_id, "call_shell");
        Ok(BTreeMap::from([(
            "OC_T40_ENV".to_owned(),
            "injected".to_owned(),
        )]))
    }
}

#[cfg(unix)]
#[tokio::test]
async fn shell_env_hook_injects_call_scoped_environment() {
    let dir = tempfile::tempdir().expect("temp dir");
    let tool = ShellTool::new(dir.path())
        .expect("shell tool")
        .with_env_hook(Arc::new(InjectEnv));

    let output = tool
        .run(
            params("printf '%s' \"$OC_T40_ENV\""),
            context(Arc::new(zuno_tool::NeverInterrupted)),
        )
        .await
        .expect("command succeeds");

    assert_eq!(output.output, "injected");
}

#[cfg(unix)]
#[tokio::test]
async fn shell_background_mode_returns_before_the_command_finishes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let marker = dir.path().join("background-finished");
    let mut input = params(format!("sleep 0.1; touch '{}'", marker.display()));
    input.background = true;
    let tool = ShellTool::new(dir.path()).expect("shell tool");
    let started = Instant::now();

    let output = tool
        .run(input, context(Arc::new(zuno_tool::NeverInterrupted)))
        .await
        .expect("background command starts");

    assert!(started.elapsed() < Duration::from_millis(100));
    assert_eq!(output.metadata["background"], true);
    wait_for_file(&marker).await;
}

#[cfg(unix)]
#[tokio::test]
async fn shell_oversized_output_is_detected_and_persisted_in_the_shared_store() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store_dir = tempfile::tempdir().expect("store dir");
    let store = ToolOutputStore::new(store_dir.path());
    let tool = ShellTool::new(dir.path())
        .expect("shell tool")
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
            .read("bash", std::path::Path::new(path))
            .expect("stored full output"),
        output.output
    );
}

#[cfg(unix)]
async fn wait_for_file(path: &Path) {
    tokio::time::timeout(Duration::from_secs(1), async {
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

//! Shell completion is a working command, not a registered placeholder.

use std::process::Command;

fn zuno() -> Command {
    Command::new(env!("CARGO_BIN_EXE_zuno"))
}

#[test]
fn completion_bash_emits_a_loadable_script() {
    let output = zuno()
        .args(["completion", "bash"])
        .output()
        .expect("run zuno completion bash");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("completion output is UTF-8");
    assert!(stdout.contains("_zuno"), "{stdout}");
    assert!(stdout.contains("complete"), "{stdout}");
}

#[test]
fn completion_requires_a_supported_shell() {
    let output = zuno()
        .arg("completion")
        .output()
        .expect("run zuno completion");

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("<SHELL>"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

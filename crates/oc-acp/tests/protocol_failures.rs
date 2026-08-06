use std::io::Write;
use std::process::{Command, Stdio};

fn run(input: &str) -> (Vec<serde_json::Value>, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_oc-acp-conformance"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("conformance agent starts");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input.as_bytes())
        .expect("request is written");
    let output = child.wait_with_output().expect("agent exits at EOF");
    assert!(output.status.success(), "agent failed: {output:?}");
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    let frames = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("stray stdout byte in {line:?}: {error}"))
        })
        .collect();
    (
        frames,
        String::from_utf8(output.stderr).expect("stderr is UTF-8"),
    )
}

#[test]
fn malformed_json_and_invalid_params_return_protocol_errors_without_crashing() {
    let (frames, stderr) = run(
        "not-json\n{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"bad\"}}\n",
    );
    assert_eq!(frames.len(), 2, "one response per malformed request");
    assert_eq!(frames[0]["id"], serde_json::Value::Null);
    assert_eq!(frames[0]["error"]["code"], -32700);
    assert_eq!(frames[1]["id"], 7);
    assert_eq!(frames[1]["error"]["code"], -32602);
    assert!(!stderr.is_empty(), "diagnostics belong on stderr");
}

#[test]
fn unknown_method_is_a_method_not_found_response() {
    let (frames, _) =
        run("{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"unknown/method\",\"params\":{}}\n");
    assert_eq!(frames[0]["id"], 9);
    assert_eq!(frames[0]["error"]["code"], -32601);
}

use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::process::{Command, ExitCode};
use std::time::Duration;

fn main() -> ExitCode {
    if let Some(code) = zuno_process::run_guard_from_args() {
        return code;
    }
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fixture failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    capture(&arguments)?;
    if arguments.first().map(String::as_str) == Some("app-server") {
        run_codex()
    } else {
        run_claude()
    }
}

fn capture(arguments: &[String]) -> Result<(), String> {
    let Some(path) = std::env::var_os("ZUNO_PRODUCT_AGENT_CAPTURE") else {
        return Ok(());
    };
    let document = json!({
        "args":arguments,
        "cwd":std::env::current_dir().map_err(|error| error.to_string())?,
        "httpProxy":std::env::var("HTTP_PROXY").ok(),
        "noProxy":std::env::var("NO_PROXY").ok(),
        "secret":std::env::var("SECRET_TOKEN").ok(),
        "threadStartRequests":[],
        "turnStartRequests":[],
    });
    std::fs::write(
        path,
        serde_json::to_vec(&document).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn capture_request(field: &str, request: &Value) -> Result<(), String> {
    let Some(path) = std::env::var_os("ZUNO_PRODUCT_AGENT_CAPTURE") else {
        return Ok(());
    };
    let mut document: Value =
        serde_json::from_slice(&std::fs::read(&path).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
    document[field]
        .as_array_mut()
        .ok_or_else(|| format!("capture field `{field}` is not an array"))?
        .push(request.clone());
    std::fs::write(
        path,
        serde_json::to_vec(&document).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn run_codex() -> Result<(), String> {
    let mode =
        std::env::var("ZUNO_PRODUCT_AGENT_FIXTURE_MODE").unwrap_or_else(|_| "normal".to_owned());
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    let mut line = String::new();
    loop {
        line.clear();
        if reader
            .read_line(&mut line)
            .map_err(|error| error.to_string())?
            == 0
        {
            return Ok(());
        }
        let request: Value =
            serde_json::from_str(line.trim_end()).map_err(|error| error.to_string())?;
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match method {
            // Never answer, so the caller is cancelled with the handshake outstanding.
            "initialize" if mode == "hang-initialize" => {
                return hang_with_child(ChildGroup::Inherited);
            }
            // The same outstanding handshake, but the product has already detached a helper that
            // inherited Zuno's stderr pipe. A group kill never reaches that helper, so the pipe
            // stays open after the guarded tree is gone and a stderr reader that was dropped rather
            // than aborted keeps looping to an EOF that will not arrive for as long as the helper
            // lives.
            "initialize" if mode == "hang-initialize-escaped" => {
                return hang_with_child(ChildGroup::Escaped);
            }
            "initialize" if mode == "incompatible" => {
                // A real installation that cannot speak the protocol usually says why on stderr, and
                // that is the only place it says it, so the reported diagnostic has to carry it.
                eprintln!("app-server: --stdio is not a recognised subcommand in this build");
                send(
                    &mut writer,
                    json!({"id":request["id"],"error":{"code":-32602,"message":"unsupported protocol version"}}),
                )?;
                return Ok(());
            }
            "initialize" => {
                send(
                    &mut writer,
                    json!({"id":request["id"],"result":{"serverInfo":{"name":"fixture"}}}),
                )?;
            }
            "initialized" => {}
            "thread/start" => {
                capture_request("threadStartRequests", &request)?;
                if mode == "hang-thread-start" {
                    return hang_with_child(ChildGroup::Inherited);
                }
                // A well-formed response that omits the one field it is defined to carry. The caller
                // cannot continue, and the exit it takes must still reap this group and settle the
                // stderr reader it started.
                if mode == "thread-start-no-id" {
                    return answer_then_hold_stderr(
                        &mut writer,
                        json!({"id":request["id"],"result":{"thread":{"unexpected":"shape"}}}),
                    );
                }
                if mode == "legacy"
                    && request.pointer("/params/sandbox").and_then(Value::as_str)
                        == Some("workspaceWrite")
                {
                    send(
                        &mut writer,
                        json!({"id":request["id"],"error":{"code":-32602,"message":"legacy enum spelling required"}}),
                    )?;
                    continue;
                }
                if mode == "approval" {
                    send(
                        &mut writer,
                        json!({
                            "id":90,
                            "method":"item/commandExecution/requestApproval",
                            "params":{"command":"echo fixture"}
                        }),
                    )?;
                    line.clear();
                    reader
                        .read_line(&mut line)
                        .map_err(|error| error.to_string())?;
                    let response: Value =
                        serde_json::from_str(line.trim_end()).map_err(|error| error.to_string())?;
                    if response.pointer("/result/decision").and_then(Value::as_str)
                        != Some("decline")
                    {
                        return Err(format!("expected unattended decline, got {response}"));
                    }
                }
                send(
                    &mut writer,
                    json!({"id":request["id"],"result":{"thread":{"id":"thr_fixture"}}}),
                )?;
                // Answer the handshake, then stop draining stdin while holding its read end open.
                // The caller's next write is the prompt, so this is the product-side half of a pipe
                // deadlock: closing the pipe instead would hand the caller a prompt EPIPE, which it
                // already handles, and would prove nothing about an unbounded write.
                if mode == "stdin-wedged" {
                    std::thread::sleep(Duration::from_secs(120));
                    return Ok(());
                }
            }
            "turn/start" => {
                capture_request("turnStartRequests", &request)?;
                // The turn request has been accepted but its response is withheld, which is the
                // one phase where the caller cannot know whether work started.
                if mode == "hang-turn-start" {
                    return hang_with_child(ChildGroup::Inherited);
                }
                // The same outstanding turn response with an escaped stderr holder, for the other
                // exit that classifies a pre-stream failure.
                if mode == "hang-turn-start-escaped" {
                    return hang_with_child(ChildGroup::Escaped);
                }
                // The turn is accepted and acknowledged without the turn id, so the caller holds
                // nothing it could interrupt. Same requirement on the exit.
                if mode == "turn-start-no-id" {
                    return answer_then_hold_stderr(
                        &mut writer,
                        json!({"id":request["id"],"result":{"turn":{"unexpected":"shape"}}}),
                    );
                }
                send(
                    &mut writer,
                    json!({"id":request["id"],"result":{"turn":{"id":"turn_fixture"}}}),
                )?;
                match mode.as_str() {
                    "malformed" => {
                        eprintln!(
                            "authorization: bearer {}",
                            std::env::var("SECRET_TOKEN").unwrap_or_default()
                        );
                        writeln!(writer, "{{not-json").map_err(|error| error.to_string())?;
                        writer.flush().map_err(|error| error.to_string())?;
                        return Ok(());
                    }
                    // A real sandbox refusal. The verdict is carried only by `codexErrorInfo`, the
                    // app-server's own typed error code: the message deliberately uses none of the
                    // vocabulary a text sniff would look for, so a classifier that reads text
                    // instead of the code reports this as a plain failure.
                    "permission-denied" => {
                        send(
                            &mut writer,
                            json!({"method":"turn/completed","params":{"turn":{"status":"failed","items":[],"error":{"message":"the sandbox rejected a write outside the workspace","codexErrorInfo":"sandboxError"}}}}),
                        )?;
                        return Ok(());
                    }
                    // A plain failure whose free-form `turn.error.message` happens to name a path
                    // containing "permissions". Nothing was refused.
                    "denied-message-text" => {
                        send(
                            &mut writer,
                            json!({"method":"turn/completed","params":{"turn":{"status":"failed","items":[],"error":{"message":"failed to write /repo/permissions/mod.rs"}}}}),
                        )?;
                        return Ok(());
                    }
                    // A plain failure with no message at all, whose only "denied" is buried in a
                    // nested value the adapter must never read as a verdict.
                    "denied-nested-field" => {
                        send(
                            &mut writer,
                            json!({"method":"turn/completed","params":{"turn":{"status":"failed","items":[],"error":{"details":{"path":"~/src/denied-cache/x"}}}}}),
                        )?;
                        return Ok(());
                    }
                    // `codexErrorInfo` also has object variants. One of those is not a refusal, and
                    // serialising it puts its field name into the payload.
                    "denied-object-code" => {
                        send(
                            &mut writer,
                            json!({"method":"turn/completed","params":{"turn":{"status":"failed","items":[],"error":{"message":"connection failed","codexErrorInfo":{"httpConnectionFailed":{"httpStatusCode":503}}}}}}),
                        )?;
                        return Ok(());
                    }
                    "stderr-denied" => {
                        eprintln!("warning: could not stat /etc/shadow: permission denied");
                        send(
                            &mut writer,
                            json!({"method":"turn/completed","params":{"turn":{"status":"failed","items":[],"error":{"message":"cargo test failed: 3 assertions"}}}}),
                        )?;
                        return Ok(());
                    }
                    // What a real native sandbox refusal actually looks like, transcribed from the
                    // installed Codex 0.150.1 driven with the parameters this adapter sends
                    // (`approvalPolicy: never`, `sandbox: workspace-write`, prompt asking it to
                    // write `/etc/zuno-denial-probe`): the turn *completed*, `error` was null, no
                    // approval was requested, stderr stayed empty, and the refusal existed only as
                    // the model's own final answer. A caller must be handed that answer, not a
                    // permission verdict, because the turn succeeded.
                    "sandbox-refusal-in-answer" => {
                        let text = "zsh:1: read-only file system: /etc/zuno-denial-probe";
                        send(
                            &mut writer,
                            json!({"method":"item/completed","params":{"item":{"type":"agentMessage","id":"msg_22a3045db25e50fca7c32c00f56a7d0e","text":text,"phase":"final_answer"}}}),
                        )?;
                        send(
                            &mut writer,
                            json!({"method":"turn/completed","params":{"turn":{"id":"01a06946-100b-7352-826d-d11cc04d9497","status":"completed","items":[{"type":"agentMessage","id":"msg_22a3045db25e50fca7c32c00f56a7d0e","text":text,"phase":"final_answer"}],"itemsView":"summary","error":null}}}),
                        )?;
                        return Ok(());
                    }
                    // A turn the product reports as `completed` while also populating a typed
                    // refusal code, with an answer that is only whitespace. The schema says `error`
                    // is populated only on a failed turn, so this frame is self-contradictory, and
                    // the frame is entirely child-authored: nothing about it may be allowed to
                    // choose a permission verdict on a turn the product says did not fail.
                    "completed-blank-denied" => {
                        let text = "   \n";
                        send(
                            &mut writer,
                            json!({"method":"item/completed","params":{"item":{"type":"agentMessage","text":text}}}),
                        )?;
                        send(
                            &mut writer,
                            json!({"method":"turn/completed","params":{"turn":{"status":"completed","items":[{"type":"agentMessage","text":text}],"error":{"message":"the sandbox rejected a write outside the workspace","codexErrorInfo":"sandboxError"}}}}),
                        )?;
                        return Ok(());
                    }
                    // A turn that states no status at all while carrying the typed refusal code.
                    // The status is what says whether the turn failed, so when it is absent the
                    // verdict is not resolvable and must not be guessed.
                    "unstated-status-denied" => {
                        send(
                            &mut writer,
                            json!({"method":"turn/completed","params":{"turn":{"items":[],"error":{"message":"connection reset by peer","codexErrorInfo":"sandboxError"}}}}),
                        )?;
                        return Ok(());
                    }
                    "hang" => return hang_with_child(ChildGroup::Inherited),
                    "hang-escaped" => return hang_with_child(ChildGroup::Escaped),
                    "eof" => return Ok(()),
                    _ => {
                        let text = if mode == "approval" {
                            "approval declined safely"
                        } else {
                            "codex final answer"
                        };
                        send(
                            &mut writer,
                            json!({"method":"item/completed","params":{"item":{"type":"agentMessage","text":text}}}),
                        )?;
                        send(
                            &mut writer,
                            json!({"method":"turn/completed","params":{"turn":{"status":"completed","items":[{"type":"agentMessage","text":text}],"error":null}}}),
                        )?;
                        return Ok(());
                    }
                }
            }
            "turn/interrupt" => return Ok(()),
            _ => {}
        }
    }
}

fn run_claude() -> Result<(), String> {
    let mode =
        std::env::var("ZUNO_PRODUCT_AGENT_FIXTURE_MODE").unwrap_or_else(|_| "normal".to_owned());
    match mode.as_str() {
        "malformed" => {
            eprintln!(
                "api_key={}",
                std::env::var("SECRET_TOKEN").unwrap_or_default()
            );
            println!("not-json");
        }
        // A real refusal. The verdict is carried only by `permission_denials`, the result message's
        // authoritative record of the tool calls the product's own permission engine refused: the
        // model-authored `result` prose deliberately uses none of the vocabulary a text sniff would
        // look for, so a classifier that reads it reports this as a plain failure.
        "permission-denied" => {
            println!(
                "{}",
                json!({"type":"result","subtype":"error_during_execution","is_error":true,"result":"the edit could not be applied","permission_denials":[{"tool_name":"Write","tool_use_id":"toolu_fixture","tool_input":{"file_path":"/repo/src/lib.rs"}}]})
            );
        }
        // A failed turn whose model-authored `result` prose mentions a permission and a denial, with
        // an empty authoritative record. Nothing was refused.
        "denied-message-text" => {
            println!(
                "{}",
                json!({"type":"result","subtype":"error_during_execution","is_error":true,"result":"I audited the permission handling in src/permissions/mod.rs and the release was denied by the linter","permission_denials":[]})
            );
        }
        // What a real native refusal actually looks like, transcribed from the installed Claude Code
        // 2.1.258 driven with the flags this adapter passes (`--permission-mode dontAsk`, prompt
        // asking it to write `/etc/zuno-denial-probe` with Bash): the tool call was refused and
        // booked in `permission_denials`, the turn itself *succeeded*
        // (`subtype: success`, `is_error: false`, `terminal_reason: completed`), the refusal text
        // was quoted back inside `result`, and stderr stayed empty. A caller must be handed that
        // answer, not a permission verdict, because the turn succeeded.
        "denied-then-answered" => {
            println!(
                "{}",
                json!({"type":"result","subtype":"success","is_error":false,"terminal_reason":"completed","num_turns":2,"result":"The command failed. Exact error returned:\n\n```\nPermission to use Bash has been denied because Claude Code is running in don't ask mode.\n```","permission_denials":[{"tool_name":"Bash","tool_use_id":"toolu_bdrk_019iEbehkhvGDiG4EDwSJq1D","tool_input":{"command":"printf hi > /etc/zuno-denial-probe","description":"Write \"hi\" to /etc/zuno-denial-probe"}}]})
            );
        }
        // The real 2.1.258 success frame above, with the answer the model happened to produce
        // reduced to whitespace. The turn still did not fail: `is_error` is false and `subtype` is
        // `success`. `permission_denials` records that one tool call was refused during the turn,
        // which is not the turn's outcome, so this must not be reported as a permission failure —
        // and the answer being blank is a missing answer, not a refusal.
        "denied-then-blank" => {
            println!(
                "{}",
                json!({"type":"result","subtype":"success","is_error":false,"num_turns":2,"result":"   \n","permission_denials":[{"tool_name":"Bash","tool_use_id":"toolu_bdrk_019iEbehkhvGDiG4EDwSJq1D","tool_input":{"command":"printf hi > /etc/zuno-denial-probe","description":"Write \"hi\" to /etc/zuno-denial-probe"}}]})
            );
        }
        "stderr-denied" => {
            eprintln!("warning: could not stat /etc/shadow: permission denied");
            println!(
                "{}",
                json!({"type":"result","subtype":"error_during_execution","is_error":true,"result":"cargo test failed: 3 assertions"})
            );
        }
        "hang" => return hang_with_child(ChildGroup::Inherited),
        "hang-escaped" => return hang_with_child(ChildGroup::Escaped),
        _ => {
            println!(
                "{}",
                json!({"type":"assistant","message":"internal stream"})
            );
            println!(
                "{}",
                json!({"type":"result","subtype":"success","is_error":false,"result":"claude final answer"})
            );
        }
    }
    Ok(())
}

fn send(writer: &mut impl Write, value: Value) -> Result<(), String> {
    serde_json::to_writer(&mut *writer, &value).map_err(|error| error.to_string())?;
    writer.write_all(b"\n").map_err(|error| error.to_string())?;
    writer.flush().map_err(|error| error.to_string())
}

/// Where the fixture's grandchild sits relative to the fixture's own process group.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ChildGroup {
    /// Inside it, like an ordinary product helper, so a group kill reaps it with the fixture.
    Inherited,
    /// Its own group, like a product that detaches an MCP server. A group kill never reaches it, so
    /// it still holds the stderr pipe it inherited after the guarded tree is gone.
    ///
    /// Unix only. The detachment is `process_group(0)`, which has no Windows analogue here, so this
    /// variant refuses to run elsewhere rather than quietly behaving like [`Self::Inherited`] and
    /// letting a Windows test pass for the wrong reason. The Windows shape of the same scenario is
    /// `taskkill /f /t` failing to reach a detached grandchild, and it is not modelled yet.
    Escaped,
}

fn hang_with_child(group: ChildGroup) -> Result<(), String> {
    wait_for_helper(detach_helper(group)?)
}

/// Answer one request with a response the caller cannot use, while an escaped helper already holds
/// the inherited stderr pipe.
///
/// The helper is detached before the response is written, so the pipe is still held at the moment the
/// caller gives up: an exit that dropped its stderr reader instead of settling it would leave that
/// reader looping, and one that skipped the reap would leave the fixture's own group alive.
fn answer_then_hold_stderr(writer: &mut impl Write, response: Value) -> Result<(), String> {
    let helper = detach_helper(ChildGroup::Escaped)?;
    send(writer, response)?;
    wait_for_helper(helper)
}

fn detach_helper(group: ChildGroup) -> Result<std::process::Child, String> {
    #[cfg(not(unix))]
    if group == ChildGroup::Escaped {
        return Err(
            "ChildGroup::Escaped is Unix only: `sh -c` and process_group(0) are POSIX, so \
                    on this platform the grandchild would stay in the guarded group and a test \
                    asserting that it escaped would pass for the wrong reason"
                .to_owned(),
        );
    }
    let mut command = Command::new("sh");
    match group {
        ChildGroup::Inherited => command.args(["-c", "sleep 300"]),
        // `exec` keeps the recorded pid on `sleep`, so a caller can release the stderr pipe by
        // signalling that one pid, and the bounded sleep stops a failing run leaking a holder.
        ChildGroup::Escaped => command.args(["-c", "exec sleep 20"]),
    };
    #[cfg(unix)]
    if group == ChildGroup::Escaped {
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
    }
    let child = command.spawn().map_err(|error| error.to_string())?;
    if let Some(path) = std::env::var_os("ZUNO_PRODUCT_AGENT_CHILD_PID") {
        std::fs::write(path, child.id().to_string()).map_err(|error| error.to_string())?;
    }
    Ok(child)
}

fn wait_for_helper(mut child: std::process::Child) -> Result<(), String> {
    loop {
        if child
            .try_wait()
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

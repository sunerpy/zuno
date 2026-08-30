//! Child-process fixture proving logs never corrupt stdout-framed protocols.

use std::io::Write as _;
use std::process::ExitCode;

use zuno_error::ToolError;
use zuno_observability::tool::ToolLifecycle;
use zuno_observability::{LogConfig, span};

fn escape(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"<invalid>\"".to_owned())
}

fn frame(body: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{body}");
    let _ = stdout.flush();
}

fn main() -> ExitCode {
    let Ok(dir) = std::env::var("ZUNO_PROBE_LOG_DIR") else {
        eprintln!("ZUNO_PROBE_LOG_DIR is required");
        return ExitCode::FAILURE;
    };

    frame(r#"{"jsonrpc":"2.0","method":"probe/start","params":{"stage":"pre-init"}}"#);

    let mut config = LogConfig::from_env(&dir)
        .with_plaintext_logs(std::env::var("ZUNO_PROBE_PLAINTEXT").is_ok_and(|value| value == "1"));
    if let Ok(directives) = std::env::var("ZUNO_PROBE_DIRECTIVES") {
        config = config.with_directives(directives);
    }
    let handle = match zuno_observability::init(config) {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("logging init failed: {error}");
            return ExitCode::FAILURE;
        }
    };

    let plaintext = handle
        .plaintext_path()
        .map(|path| escape(path.to_string_lossy().as_ref()))
        .unwrap_or_else(|| "null".to_owned());
    frame(&format!(
        r#"{{"jsonrpc":"2.0","method":"probe/ready","params":{{"level":"{level}","print_logs":{print_logs},"plaintext_logs":{plaintext_logs},"installed":{installed},"database":{database},"plaintext":{plaintext},"process_uuid":{process_uuid},"pid":{pid}}}}}"#,
        level = handle.level().as_str(),
        print_logs = handle.print_logs(),
        plaintext_logs = handle.plaintext_logs(),
        installed = handle.installed(),
        database = escape(handle.database_path().to_string_lossy().as_ref()),
        process_uuid = handle
            .process_uuid()
            .map(escape)
            .unwrap_or_else(|| "null".to_owned()),
        pid = handle.process_id().unwrap_or_default(),
    ));

    let second = match zuno_observability::init(LogConfig::from_env(&dir)) {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("second logging init failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    frame(&format!(
        r#"{{"jsonrpc":"2.0","method":"probe/second-init","params":{{"installed":{}}}}}"#,
        second.installed()
    ));

    let turn_span = span::turn("ses_probe", "turn_probe");
    span::record_turn_identity(&turn_span, "build", "anthropic", "claude-sonnet-4-5");
    let _turn_entered = turn_span.enter();

    tracing::trace!(marker = "probe-trace", "probe emitted at trace");
    tracing::debug!(marker = "probe-debug", "probe emitted at debug");
    tracing::info!(marker = "probe-info", "probe emitted at info");
    tracing::warn!(marker = "probe-warn", "probe emitted at warn");
    tracing::error!(marker = "probe-error", "probe emitted at error");
    tracing::info!(
        marker = "probe-sensitive",
        command = "never-store-this-command",
        command_bytes = 24,
        "probe emitted a sensitive field"
    );

    {
        let request_span = span::provider_request("anthropic", "claude-sonnet-4-5", 1, true);
        let _request_entered = request_span.enter();
        tracing::info!(
            marker = "probe-provider",
            "probe emitted inside provider_request"
        );
        span::record_provider_response(&request_span, 200, Some("req_probe"));
        span::record_provider_outcome(&request_span, "completed", None, Some(200));
        tracing::debug!(
            marker = "probe-provider-finished",
            "probe provider request finished"
        );
    }

    {
        let tool_span = span::tool_call("shell", "toolu_probe");
        let _tool_entered = tool_span.enter();
        let mut call = ToolLifecycle::pending("shell", "toolu_probe");
        call.running();
        call.completed();

        let failing = ToolLifecycle::pending("shell", "toolu_probe_err");
        failing.failed(&ToolError::NotFound {
            tool: "shell".to_owned(),
        });

        let blocked = ToolLifecycle::pending("shell", "toolu_probe_blocked");
        blocked.blocked("denied");

        let _abandoned = ToolLifecycle::pending("shell", "toolu_probe_abandoned");
    }

    frame(r#"{"jsonrpc":"2.0","method":"probe/emitted","params":{"levels":5}}"#);
    frame(&format!(
        r#"{{"jsonrpc":"2.0","id":1,"result":{{"dropped_records":{},"write_failures":{}}}}}"#,
        handle.dropped_lines().unwrap_or_default(),
        handle.write_failures().unwrap_or_default(),
    ));

    drop(second);
    drop(handle);
    ExitCode::SUCCESS
}

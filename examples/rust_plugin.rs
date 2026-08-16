use std::error::Error;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;
use zuno_plugin_sdk::{
    ConformanceSuite, HandlerError, HookCase, Plugin, ToolCase, ToolDefinition, ToolOutput,
};

fn plugin() -> Result<Plugin, Box<dyn Error>> {
    let id = std::env::var("OC_EXAMPLE_PLUGIN_ID").unwrap_or_else(|_| "rust-example".to_owned());
    let operation = std::env::var("OC_EXAMPLE_OPERATION").unwrap_or_else(|_| "add".to_owned());
    let sleep_ms = std::env::var("OC_EXAMPLE_SLEEP_HOOK_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let text_id = id.clone();
    let plugin = Plugin::new(id)
        .tool(
            ToolDefinition::new(
                "rust_echo",
                "Echo text from a Rust plugin",
                json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"],
                }),
            ),
            |call| async move {
                let text = call.arguments["text"]
                    .as_str()
                    .ok_or_else(|| HandlerError::new("text must be a string"))?;
                Ok(ToolOutput::text("Rust echo", text))
            },
        )?
        .hook("chat.params", move |mut call| {
            let operation = operation.clone();
            async move {
                if sleep_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
                }
                let temperature = call.output["temperature"]
                    .as_f64()
                    .ok_or_else(|| HandlerError::new("temperature must be numeric"))?;
                call.output["temperature"] = json!(if operation == "multiply" {
                    temperature * 10.0
                } else {
                    temperature + 1.0
                });
                Ok(call)
            }
        })?
        .hook("shell.env", |mut call| async move {
            call.output["env"]["RUST_PLUGIN"] = json!("enabled");
            Ok(call)
        })?
        .hook("experimental.text.complete", move |mut call| {
            let text_id = text_id.clone();
            async move {
                if sleep_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
                }
                let text = call.output["text"]
                    .as_str()
                    .ok_or_else(|| HandlerError::new("text must be a string"))?;
                let suffix = match text_id.as_str() {
                    "first" => "A",
                    "second" => "B",
                    _ => "!",
                };
                call.output["text"] = json!(format!("{text}{suffix}"));
                Ok(call)
            }
        })?;
    Ok(plugin)
}

async fn wait_for_startup_gate() -> Result<(), Box<dyn Error>> {
    let Ok(directory) = std::env::var("OC_EXAMPLE_STARTUP_GATE") else {
        return Ok(());
    };
    let id = std::env::var("OC_EXAMPLE_PLUGIN_ID")?;
    let count = std::env::var("OC_EXAMPLE_GATE_COUNT")?.parse::<usize>()?;
    std::fs::create_dir_all(&directory)?;
    std::fs::write(Path::new(&directory).join(format!("{id}.ready")), b"ready")?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let ready = std::fs::read_dir(&directory)?
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|value| value == "ready")
            })
            .count();
        if ready >= count {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("startup gate deadline expired".into());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn conformance() -> ConformanceSuite {
    ConformanceSuite::new()
        .tool(ToolCase::new(
            "rust_echo",
            json!({ "text": "hello" }),
            ToolOutput::text("Rust echo", "hello"),
        ))
        .hook(HookCase::new(
            "chat.params",
            json!({}),
            json!({ "temperature": 1.0 }),
            json!({ "temperature": 2.0 }),
        ))
        .hook(HookCase::new(
            "shell.env",
            json!({}),
            json!({ "env": {} }),
            json!({ "env": { "RUST_PLUGIN": "enabled" } }),
        ))
        .hook(HookCase::new(
            "experimental.text.complete",
            json!({}),
            json!({ "text": "done" }),
            json!({ "text": "done!" }),
        ))
}

#[tokio::main]
async fn main() {
    if std::env::var_os("OC_EXAMPLE_PANIC_STARTUP").is_some() {
        panic!("requested startup panic");
    }
    if let Err(error) = run().await {
        eprintln!("example plugin failed: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let plugin = plugin()?;
    if std::env::args().any(|argument| argument == "--conformance") {
        let report = conformance().run(&plugin).await?;
        eprintln!(
            "conformance passed: {} hooks, {} tool",
            report.hooks_checked, report.tools_checked
        );
        return Ok(());
    }
    wait_for_startup_gate().await?;
    zuno_plugin_sdk::serve(plugin).await?;
    Ok(())
}

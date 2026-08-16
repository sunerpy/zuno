use async_trait::async_trait;
use serde_json::{Value, json};
use std::error::Error as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use zuno_error::ToolError;
use zuno_tool::{
    NeverInterrupted, PermissionAsk, PermissionAsker, Tool, ToolContext, ToolOutput, erase,
};
use zuno_tools::registry::{
    BuiltinSlot, CustomTool, CustomToolLoader, RegistryFlags, ToolRegistry, ToolRegistryBuilder,
};
use zuno_tools::{FileTools, GrepTool, SearchTooling};

const MAX_CALLS: usize = 10;

struct FixedTools(Vec<CustomTool>);

impl CustomToolLoader for FixedTools {
    fn config_directory_tools(&self, _directories: &[std::path::PathBuf]) -> Vec<CustomTool> {
        self.0.clone()
    }

    fn plugin_tools(&self) -> Vec<CustomTool> {
        Vec::new()
    }
}

struct ProbeTool {
    id: &'static str,
    in_flight: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for ProbeTool {
    fn id(&self) -> &str {
        self.id
    }

    fn description(&self) -> &str {
        "Test probe."
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "label": { "type": "string" },
                "delay_ms": { "type": "integer" }
            },
            "required": ["label"]
        })
    }

    async fn execute(&self, args: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let label =
            args.get("label")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidArgs {
                    tool: self.id.to_owned(),
                    source: Box::new(std::io::Error::other("label is required")),
                })?;
        let delay_ms = args.get("delay_ms").and_then(Value::as_u64).unwrap_or(0);
        let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(current, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(ToolOutput::text(label, label).with_metadata("depth", ctx.depth))
    }
}

struct LargeTool;

#[async_trait]
impl Tool for LargeTool {
    fn id(&self) -> &str {
        "large"
    }

    fn description(&self) -> &str {
        "Return a large fixture."
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({ "type": "object" })
    }

    async fn execute(&self, _args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("large", "x".repeat(50_001)))
    }
}

struct DenyNamed(&'static str);

#[async_trait]
impl PermissionAsker for DenyNamed {
    async fn ask(&self, tool: &str, _ask: PermissionAsk) -> Result<(), ToolError> {
        if tool == self.0 {
            Err(ToolError::Denied {
                tool: tool.to_owned(),
            })
        } else {
            Ok(())
        }
    }
}

fn context(permission: Arc<dyn PermissionAsker>) -> ToolContext {
    ToolContext::new(
        "ses_batch",
        "msg_batch",
        "call_batch",
        "build",
        permission,
        Arc::new(NeverInterrupted),
    )
}

fn registry(root: &Path, tools: Vec<CustomTool>) -> ToolRegistry {
    let files = FileTools::new(root).expect("create file tools");
    ToolRegistryBuilder::new(
        root,
        Some(root.to_path_buf()),
        files,
        RegistryFlags {
            experimental_code_mode: true,
            ..RegistryFlags::default()
        },
    )
    .with_custom_loader(Arc::new(FixedTools(tools)))
    .build()
}

fn probe(id: &'static str) -> (CustomTool, Arc<AtomicUsize>) {
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_in_flight = Arc::new(AtomicUsize::new(0));
    (
        Arc::new(ProbeTool {
            id,
            in_flight,
            max_in_flight: Arc::clone(&max_in_flight),
        }),
        max_in_flight,
    )
}

fn source(error: &ToolError) -> String {
    error
        .source()
        .map(ToString::to_string)
        .unwrap_or_else(|| error.to_string())
}

#[test]
fn batch_schema_requires_intent_and_keeps_subtool_arguments_inline() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let registry = registry(root.path(), Vec::new());
    let definition = registry
        .all()
        .iter()
        .find(|tool| tool.id() == "execute")
        .expect("execute is registered")
        .definition();

    let calls = &definition.parameters["properties"]["tool_calls"];
    assert_eq!(calls["maxItems"], MAX_CALLS);
    assert_eq!(calls["items"]["required"], json!(["tool", "intent"]));
    assert_eq!(calls["items"]["additionalProperties"], true);
}

#[tokio::test]
async fn batch_ten_independent_calls_run_concurrently_and_render_in_submission_order() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let (tool, max_in_flight) = probe("probe");
    let registry = registry(root.path(), vec![tool]);
    let calls: Vec<Value> = (0..MAX_CALLS)
        .map(|index| {
            json!({
                "tool": "probe",
                "intent": "measure bounded parallelism",
                "label": format!("result-{index}"),
                "delay_ms": 40 + (MAX_CALLS - index) as u64
            })
        })
        .collect();

    let started = Instant::now();
    let output = registry
        .execute(
            "execute",
            json!({ "tool_calls": calls }),
            context(Arc::new(zuno_tool::AllowAll)),
        )
        .await
        .expect("batch succeeds");

    assert!(started.elapsed() < Duration::from_millis(250));
    assert_eq!(max_in_flight.load(Ordering::SeqCst), MAX_CALLS);
    let positions: Vec<usize> = (0..MAX_CALLS)
        .map(|index| {
            output
                .output
                .find(&format!("--- [{}] probe ---", index + 1))
                .expect("submission header")
        })
        .collect();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
}

#[tokio::test]
async fn batch_six_call_parallelism_is_stable_across_three_rounds() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let (tool, max_in_flight) = probe("probe");
    let registry = registry(root.path(), vec![tool]);

    for round in 0..3 {
        max_in_flight.store(0, Ordering::SeqCst);
        let calls: Vec<Value> = (0..6)
            .map(|index| {
                json!({
                    "tool": "probe",
                    "intent": "repeat concurrency proof",
                    "label": format!("round-{round}-result-{index}"),
                    "delay_ms": 30
                })
            })
            .collect();
        let output = registry
            .execute(
                "execute",
                json!({ "tool_calls": calls }),
                context(Arc::new(zuno_tool::AllowAll)),
            )
            .await
            .expect("parallel round succeeds");

        assert_eq!(max_in_flight.load(Ordering::SeqCst), 6);
        assert!(output.output.contains("Completed: 6 succeeded, 0 failed"));
    }
}

#[tokio::test]
async fn batch_an_eleventh_declared_call_is_refused_with_the_cap() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let (tool, _) = probe("probe");
    let registry = registry(root.path(), vec![tool]);
    let calls: Vec<Value> = (0..=MAX_CALLS)
        .map(|index| json!({ "tool": "probe", "intent": "overflow", "label": index.to_string() }))
        .collect();

    let error = registry
        .execute(
            "execute",
            json!({ "tool_calls": calls }),
            context(Arc::new(zuno_tool::AllowAll)),
        )
        .await
        .expect_err("the cap is enforced");

    assert!(matches!(error, ToolError::InvalidArgs { .. }));
    assert!(source(&error).contains("10"));
}

#[tokio::test]
async fn batch_recursion_is_refused_after_alias_resolution() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let registry = registry(root.path(), Vec::new());

    let error = registry
        .execute(
            "execute",
            json!({
                "tool_calls": [{
                    "tool": "functions.Execute",
                    "intent": "must not recurse"
                }]
            }),
            context(Arc::new(zuno_tool::AllowAll)),
        )
        .await
        .expect_err("execute cannot batch itself");

    assert!(source(&error).contains("Cannot execute the `execute` tool"));
}

#[tokio::test]
async fn batch_a_denied_subcall_is_an_error_while_its_sibling_succeeds() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let (allowed, _) = probe("allowed");
    let (blocked, _) = probe("blocked");
    let registry = registry(root.path(), vec![allowed, blocked]);

    let output = registry
        .execute(
            "execute",
            json!({
                "tool_calls": [
                    { "tool": "blocked", "intent": "prove denial isolation", "label": "no" },
                    { "tool": "allowed", "intent": "prove sibling success", "label": "yes" }
                ]
            }),
            context(Arc::new(DenyNamed("blocked"))),
        )
        .await
        .expect("a subcall failure does not fail the batch");

    assert!(output.output.contains("Error: tool blocked was denied"));
    assert!(output.output.contains("yes"));
    assert!(output.output.contains("Completed: 1 succeeded, 1 failed"));
}

#[tokio::test]
async fn batch_a_reference_consumes_an_earlier_output_without_a_second_turn() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let (tool, _) = probe("probe");
    let registry = registry(root.path(), vec![tool]);

    let output = registry
        .execute(
            "execute",
            json!({
                "tool_calls": [
                    {
                        "tool": "probe",
                        "intent": "produce a value",
                        "bind": "first",
                        "label": "bound-value"
                    },
                    {
                        "tool": "probe",
                        "intent": "consume the value",
                        "label": { "$ref": "first.output" }
                    }
                ]
            }),
            context(Arc::new(zuno_tool::AllowAll)),
        )
        .await
        .expect("the dependent call succeeds");

    assert!(
        output
            .output
            .contains("(bound as `first`; output withheld)")
    );
    assert!(output.output.contains("bound-value"));
    assert!(output.output.contains("Completed: 2 succeeded, 0 failed"));
}

#[tokio::test]
async fn batch_grep_fans_out_into_real_reads_without_exposing_the_match_list() {
    let root = tempfile::tempdir().expect("temporary workspace");
    std::fs::create_dir(root.path().join("src")).expect("fixture directory");
    std::fs::write(root.path().join("src/a.txt"), "needle alpha\n").expect("fixture a");
    std::fs::write(root.path().join("src/b.txt"), "needle beta\n").expect("fixture b");
    let files = FileTools::new(root.path()).expect("create file tools");
    let mut builder = ToolRegistryBuilder::new(
        root.path(),
        Some(root.path().to_path_buf()),
        files,
        RegistryFlags {
            experimental_code_mode: true,
            ..RegistryFlags::default()
        },
    );
    builder
        .register_builtin(
            BuiltinSlot::Grep,
            erase(GrepTool::new(SearchTooling::new(root.path()))),
        )
        .expect("register grep");
    let registry = builder.build();

    let output = registry
        .execute(
            "execute",
            json!({
                "tool_calls": [
                    {
                        "tool": "grep",
                        "intent": "find fixtures",
                        "bind": "hits",
                        "pattern": "needle",
                        "path": "src"
                    },
                    {
                        "tool": "read",
                        "intent": "read every matching file",
                        "filePath": { "$each": "hits.metadata.files[*]" }
                    }
                ]
            }),
            context(Arc::new(zuno_tool::AllowAll)),
        )
        .await
        .expect("grep and reads complete in one call");

    assert!(!output.output.contains("Found 2 matches"));
    assert!(output.output.contains("needle alpha"));
    assert!(output.output.contains("needle beta"));
    assert!(output.output.contains("Completed: 3 succeeded, 0 failed"));
}

#[tokio::test]
async fn batch_a_missing_binding_is_refused_and_names_the_available_bindings() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let (tool, _) = probe("probe");
    let registry = registry(root.path(), vec![tool]);

    let error = registry
        .execute(
            "execute",
            json!({
                "tool_calls": [
                    { "tool": "probe", "intent": "produce", "bind": "first", "label": "value" },
                    { "tool": "probe", "intent": "consume", "label": { "$ref": "missing.output" } }
                ]
            }),
            context(Arc::new(zuno_tool::AllowAll)),
        )
        .await
        .expect_err("an unknown binding is invalid input");

    let message = source(&error);
    assert!(message.contains("missing"));
    assert!(message.contains("Available bindings: first"));
}

#[tokio::test]
async fn batch_oversized_subcall_output_is_persisted_and_refused_not_truncated() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let registry = registry(root.path(), vec![Arc::new(LargeTool)]);

    let output = registry
        .execute(
            "execute",
            json!({
                "tool_calls": [{ "tool": "large", "intent": "exercise budget" }]
            }),
            context(Arc::new(zuno_tool::AllowAll)),
        )
        .await
        .expect("the batch reports a per-call refusal");

    assert!(output.output.contains("Full output saved to"));
    assert!(!output.output.contains("(truncated)"));
    assert!(output.output.contains("Completed: 0 succeeded, 1 failed"));
}

use std::error::Error as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use url::Url;
use zuno_engine::terminal_lease::{TerminalBroker, TerminalLease};
use zuno_paths::ResolvedProject;
use zuno_plugin::{ConfigToolDiagnosticKind, JsHostConfig, load_config_directory_tools};
use zuno_testkit::FakeTerminalOwner;
use zuno_tool::{AllowAll, NeverInterrupted, ToolContext};

fn host(root: &Path) -> JsHostConfig {
    let owner = Arc::new(FakeTerminalOwner::new());
    let terminal: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::new(owner));
    JsHostConfig::new(
        ResolvedProject {
            previous: None,
            id: "config-tools-fixture".to_owned(),
            directory: root.to_path_buf(),
            vcs: None,
        },
        Url::parse("http://127.0.0.1:4096").expect("server URL"),
        terminal,
    )
    .directory(root)
    .worktree(root)
    .cache_dir(root.join("cache"))
}

fn context() -> ToolContext {
    ToolContext::new(
        "session",
        "message",
        "call",
        "build",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
}

fn write_tool(root: &Path, directory: &str, file: &str, source: &str) -> PathBuf {
    let directory = root.join(directory);
    std::fs::create_dir_all(&directory).expect("tool directory");
    let path = directory.join(file);
    std::fs::write(&path, source).expect("tool fixture");
    path
}

fn real_zod() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("OPENCODE_ZOD_FIXTURE") {
        let path = PathBuf::from(path);
        return path.join("package.json").is_file().then_some(path);
    }
    let candidates = [
        PathBuf::from("/config/workspace/ProdDir/AI/opencode/packages/opencode/node_modules/zod"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../opencode/packages/opencode/node_modules/zod"),
    ];
    candidates
        .into_iter()
        .find(|path| path.join("package.json").is_file())
}

#[cfg(unix)]
fn install_zod(root: &Path, zod: &Path) {
    use std::os::unix::fs::symlink;

    let modules = root.join("node_modules");
    std::fs::create_dir_all(&modules).expect("node_modules");
    symlink(zod, modules.join("zod")).expect("link real Zod fixture");
}

#[cfg(not(unix))]
fn install_zod(_root: &Path, _zod: &Path) {
    unreachable!("the caller skips this fixture without symlink support");
}

#[tokio::test]
async fn config_tools_real_zod_schema_executes_and_validates_in_javascript() {
    if zuno_plugin::discover_runtime(&["config-tools-zod-fixture".to_owned()]).is_err() {
        eprintln!("SKIP config_tools_real_zod_schema: neither bun nor node is available");
        return;
    }
    let Some(zod) = real_zod() else {
        eprintln!(
            "SKIP config_tools_real_zod_schema: no real Zod v4 package; set OPENCODE_ZOD_FIXTURE"
        );
        return;
    };
    #[cfg(not(unix))]
    {
        eprintln!("SKIP config_tools_real_zod_schema: fixture requires symlink support");
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    install_zod(temp.path(), &zod);
    write_tool(
        temp.path(),
        "tool",
        "weather.ts",
        r#"
import { z } from "zod";

export default {
  description: "Return fixture weather.",
  args: {
    city: z.string().describe("City to inspect"),
    unit: z.enum(["c", "f"]).optional().describe("Temperature unit"),
  },
  async execute(args, context) {
    context.metadata({ title: "Weather lookup", metadata: { agent: context.agent } });
    return `${args.city}:${args.unit ?? "c"}`;
  },
};
"#,
    );

    let load = load_config_directory_tools(&[temp.path().to_path_buf()], host(temp.path())).await;
    assert!(load.diagnostics().is_empty(), "{:?}", load.diagnostics());
    let tool = load
        .tools()
        .iter()
        .find(|tool| tool.id() == "weather")
        .expect("default export uses the basename");
    let schema = tool.raw_parameters_schema();
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["properties"]["city"]["type"], "string");
    assert_eq!(
        schema["properties"]["city"]["description"],
        "City to inspect"
    );
    assert_eq!(schema["properties"]["unit"]["enum"], json!(["c", "f"]));
    assert_eq!(schema["required"], json!(["city"]));

    let output = tool
        .execute(json!({ "city": "Oslo", "unit": "f" }), context())
        .await
        .expect("valid Zod input executes");
    assert_eq!(output.title, "Weather lookup");
    assert_eq!(output.output, "Oslo:f");
    assert_eq!(output.metadata["agent"], "build");

    let error = tool
        .execute(json!({ "city": "Oslo", "unit": "kelvin" }), context())
        .await
        .expect_err("Zod validation must remain in JavaScript");
    assert_eq!(error.tool(), "weather");
    assert!(matches!(error, zuno_error::ToolError::InvalidArgs { .. }));
    assert!(
        error
            .source()
            .is_some_and(|source| source.to_string().contains("expected one of")),
        "{error:?}"
    );
    load.shutdown().await;
}

#[tokio::test]
async fn config_tools_non_zod_shape_falls_back_and_named_export_is_namespaced() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_tool(
        temp.path(),
        "tools",
        "legacy.js",
        r#"
export const ping = {
  description: "Legacy schema fixture.",
  args: {
    name: { type: "string", description: "Who to ping" },
    ignored: 42,
  },
  async execute(args) {
    return { title: "Legacy", output: `pong ${args.name}`, metadata: { legacy: true } };
  },
};
"#,
    );

    let load = load_config_directory_tools(&[temp.path().to_path_buf()], host(temp.path())).await;
    assert!(load.diagnostics().is_empty(), "{:?}", load.diagnostics());
    let tool = load
        .tools()
        .iter()
        .find(|tool| tool.id() == "legacy_ping")
        .expect("named export is namespaced");
    let schema = tool.raw_parameters_schema();
    assert_eq!(schema["properties"]["name"]["type"], "string");
    assert!(schema["properties"].get("ignored").is_none());
    assert_eq!(schema["required"], json!(["name"]));

    let output = tool
        .execute(json!({ "name": "Ada" }), context())
        .await
        .expect("legacy tool executes");
    assert_eq!(output.title, "Legacy");
    assert_eq!(output.output, "pong Ada");
    assert_eq!(output.metadata["legacy"], true);
    load.shutdown().await;
}

#[tokio::test]
async fn config_tools_builtin_collision_is_reported_instead_of_shadowing() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_tool(
        temp.path(),
        "tool",
        "bash.js",
        r#"
export default {
  description: "Must not shadow bash.",
  args: { command: { type: "string" } },
  async execute(args) { return args.command; },
};
"#,
    );

    let load = load_config_directory_tools(&[temp.path().to_path_buf()], host(temp.path())).await;
    let error = load
        .validate_tool_names(["bash", "read", "write"])
        .expect_err("a built-in collision must be explicit");
    assert_eq!(
        error.to_string(),
        "plugin tool `bash` conflicts with a reserved tool name"
    );
    load.shutdown().await;
}

#[tokio::test]
async fn config_tools_duplicate_name_reports_both_source_paths() {
    let first = tempfile::tempdir().expect("first config directory");
    let second = tempfile::tempdir().expect("second config directory");
    let source = r#"
export default {
  description: "Duplicate fixture.",
  args: { value: { type: "string" } },
  async execute(args) { return args.value; },
};
"#;
    let first_path = write_tool(first.path(), "tool", "duplicate.js", source);
    let second_path = write_tool(second.path(), "tools", "duplicate.js", source);
    let directories = [first.path().to_path_buf(), second.path().to_path_buf()];

    let load = load_config_directory_tools(&directories, host(first.path())).await;
    let error = load
        .validate_tool_names(std::iter::empty())
        .expect_err("duplicate config tool names must be explicit");
    let rendered = error.to_string();

    assert!(rendered.contains("duplicate"), "{rendered}");
    assert!(
        rendered.contains(&first_path.display().to_string()),
        "{rendered}"
    );
    assert!(
        rendered.contains(&second_path.display().to_string()),
        "{rendered}"
    );
    load.shutdown().await;
}

#[tokio::test]
async fn config_tools_throwing_import_names_the_file_and_does_not_hide_siblings() {
    let temp = tempfile::tempdir().expect("tempdir");
    let bad = write_tool(
        temp.path(),
        "tool",
        "bad.js",
        r#"throw new Error("fixture import exploded");"#,
    );
    write_tool(
        temp.path(),
        "tools",
        "good.js",
        r#"
export default {
  description: "Healthy sibling.",
  args: { value: { type: "string" } },
  async execute(args) { return args.value; },
};
"#,
    );

    let load = load_config_directory_tools(&[temp.path().to_path_buf()], host(temp.path())).await;
    assert_eq!(load.tools().len(), 1);
    assert_eq!(load.tools()[0].id(), "good");
    assert_eq!(load.diagnostics().len(), 1);
    assert_eq!(load.diagnostics()[0].path, bad);
    assert_eq!(
        load.diagnostics()[0].kind,
        ConfigToolDiagnosticKind::FailedToLoad
    );
    assert!(
        load.diagnostics()[0]
            .message
            .contains("fixture import exploded"),
        "{:?}",
        load.diagnostics()[0]
    );
    load.shutdown().await;
}

use async_trait::async_trait;
use oc_error::ToolError;
use oc_permission::{PermissionAction, Rule};
use oc_testkit::pinned_oracle_or_skip;
use oc_tool::{Tool, ToolContext, ToolOutput};
use oc_tools::FileTools;
use oc_tools::SearchConfig;
use oc_tools::exposure::ExposureFlags;
use oc_tools::registry::{
    BuiltinSlot, CustomTool, CustomToolLoader, McpToolLoader, RegistryError, RegistryFlags,
    ResolveInput, ToolRegistry, ToolRegistryBuilder, ToolSource, config_tool_id,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

struct StubTool(&'static str);

#[async_trait]
impl Tool for StubTool {
    fn id(&self) -> &str {
        self.0
    }

    fn description(&self) -> &str {
        "registry test tool"
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({ "type": "object" })
    }

    async fn execute(&self, _args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text(self.0, "ok"))
    }
}

fn stub(id: &'static str) -> Arc<dyn Tool> {
    Arc::new(StubTool(id))
}

struct TaggedTool {
    id: &'static str,
    description: &'static str,
}

#[async_trait]
impl Tool for TaggedTool {
    fn id(&self) -> &str {
        self.id
    }

    fn description(&self) -> &str {
        self.description
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({ "type": "object" })
    }

    async fn execute(&self, _args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text(self.description, "ok"))
    }
}

fn tagged(id: &'static str, description: &'static str) -> Arc<dyn Tool> {
    Arc::new(TaggedTool { id, description })
}

fn register_non_file_builtins(builder: &mut ToolRegistryBuilder) {
    for (slot, id) in [
        (BuiltinSlot::Invalid, "invalid"),
        (BuiltinSlot::Question, "question"),
        (BuiltinSlot::Shell, "bash"),
        (BuiltinSlot::Glob, "glob"),
        (BuiltinSlot::Grep, "grep"),
        (BuiltinSlot::Task, "task"),
        (BuiltinSlot::Fetch, "webfetch"),
        (BuiltinSlot::Todo, "todowrite"),
        (BuiltinSlot::Search, "websearch"),
        (BuiltinSlot::Skill, "skill"),
        (BuiltinSlot::Execute, "execute"),
        (BuiltinSlot::Lsp, "lsp"),
        (BuiltinSlot::Plan, "plan_exit"),
    ] {
        builder
            .register_builtin(slot, stub(id))
            .expect("the stub id belongs to its slot");
    }
}

fn registry(root: &Path, flags: RegistryFlags) -> ToolRegistry {
    let files = FileTools::new(root).expect("create file tools");
    let mut builder = ToolRegistryBuilder::new(root, Some(root.to_path_buf()), files, flags);
    register_non_file_builtins(&mut builder);
    builder.build()
}

fn ids(tools: &[Arc<dyn Tool>]) -> Vec<&str> {
    tools.iter().map(|tool| tool.id()).collect()
}

fn deny(permission: &str, pattern: &str) -> Rule {
    Rule {
        permission: permission.to_owned(),
        pattern: pattern.to_owned(),
        action: PermissionAction::Deny,
    }
}

#[test]
fn registry_builtin_order_matches_the_oracle_before_turn_filters() {
    let root = TempDir::new().expect("temporary workspace");
    let registry = registry(root.path(), RegistryFlags::default());

    assert_eq!(
        ids(registry.all()),
        vec![
            "invalid",
            "question",
            "bash",
            "read",
            "glob",
            "grep",
            "edit",
            "write",
            "task",
            "webfetch",
            "todowrite",
            "websearch",
            "skill",
            "apply_patch",
        ]
    );
}

#[test]
fn registry_model_family_uses_the_shared_file_tool_projection() {
    let root = TempDir::new().expect("temporary workspace");
    let registry = registry(root.path(), RegistryFlags::default());

    for model in ["claude-sonnet-4-5", "gpt-4.1", "gpt-oss-120b"] {
        let offered = registry.resolved_ids(ResolveInput::new(model, "openai", &[]));
        assert!(offered.contains(&"edit".to_owned()), "{model}");
        assert!(offered.contains(&"write".to_owned()), "{model}");
        assert!(!offered.contains(&"apply_patch".to_owned()), "{model}");
    }

    let offered = registry.resolved_ids(ResolveInput::new("gpt-5.2", "openai", &[]));
    assert!(offered.contains(&"apply_patch".to_owned()));
    assert!(!offered.contains(&"edit".to_owned()));
    assert!(!offered.contains(&"write".to_owned()));
}

#[test]
fn registry_full_deny_hides_a_tool_but_a_narrow_deny_keeps_it_visible() {
    let root = TempDir::new().expect("temporary workspace");
    let registry = registry(root.path(), RegistryFlags::default());

    let fully_denied = registry.resolved_ids(ResolveInput::new(
        "claude-sonnet-4-5",
        "anthropic",
        &[deny("bash", "*")],
    ));
    assert!(!fully_denied.contains(&"bash".to_owned()));

    let narrowly_denied = registry.resolved_ids(ResolveInput::new(
        "claude-sonnet-4-5",
        "anthropic",
        &[deny("bash", "git push*")],
    ));
    assert!(narrowly_denied.contains(&"bash".to_owned()));
}

#[test]
fn registry_execute_requires_both_code_mode_and_a_resolved_description() {
    let root = TempDir::new().expect("temporary workspace");
    let registry = registry(
        root.path(),
        RegistryFlags {
            experimental_code_mode: true,
            ..RegistryFlags::default()
        },
    );
    assert!(ids(registry.all()).contains(&"execute"));

    let without_catalog = registry.resolved_ids(ResolveInput::new("gpt-5.2", "openai", &[]));
    assert!(!without_catalog.contains(&"execute".to_owned()));

    let with_catalog = registry.resolved_ids(
        ResolveInput::new("gpt-5.2", "openai", &[])
            .with_code_mode_description("one connected MCP tool"),
    );
    assert!(with_catalog.contains(&"execute".to_owned()));
}

#[test]
fn registry_config_export_ids_follow_default_and_named_rules() {
    let path = Path::new("/config/.opencode/tools/release.notes.ts");
    assert_eq!(
        config_tool_id(path, "default").as_deref(),
        Some("release.notes")
    );
    assert_eq!(
        config_tool_id(path, "publish").as_deref(),
        Some("release.notes_publish")
    );
    assert_eq!(config_tool_id(Path::new("/"), "default"), None);
}

struct FixedCustomLoader {
    seen_directories: Arc<Mutex<Vec<PathBuf>>>,
}

impl CustomToolLoader for FixedCustomLoader {
    fn config_directory_tools(&self, directories: &[PathBuf]) -> Vec<CustomTool> {
        *self
            .seen_directories
            .lock()
            .expect("record config directories") = directories.to_vec();
        vec![stub("config_default"), stub("config_named")]
    }

    fn plugin_tools(&self) -> Vec<CustomTool> {
        vec![stub("plugin_tool")]
    }
}

struct FixedMcpLoader;

impl McpToolLoader for FixedMcpLoader {
    fn tools(&self) -> Vec<CustomTool> {
        vec![stub("mcp_tool")]
    }
}

struct CollidingCustomLoader;

impl CustomToolLoader for CollidingCustomLoader {
    fn config_directory_tools(&self, _directories: &[PathBuf]) -> Vec<CustomTool> {
        vec![tagged("grep", "config-directory grep")]
    }

    fn plugin_tools(&self) -> Vec<CustomTool> {
        vec![tagged("grep", "plugin grep")]
    }
}

struct CollidingMcpLoader;

impl McpToolLoader for CollidingMcpLoader {
    fn tools(&self) -> Vec<CustomTool> {
        vec![tagged("grep", "MCP grep")]
    }
}

#[test]
fn registry_de_duplicates_cross_source_names_with_upstreams_last_source_winning() {
    let root = TempDir::new().expect("temporary workspace");
    let files = FileTools::new(root.path()).expect("create file tools");
    let mut builder = ToolRegistryBuilder::new(
        root.path(),
        Some(root.path().to_path_buf()),
        files,
        RegistryFlags::default(),
    );
    register_non_file_builtins(&mut builder);
    let registry = builder
        .with_custom_loader(Arc::new(CollidingCustomLoader))
        .with_mcp_loader(Arc::new(CollidingMcpLoader))
        .build();

    let grep = registry
        .all()
        .iter()
        .filter(|tool| tool.id() == "grep")
        .collect::<Vec<_>>();
    assert_eq!(grep.len(), 1, "provider-facing tool ids must be unique");
    assert_eq!(grep[0].description(), "MCP grep");
    assert_eq!(
        registry
            .diagnostics()
            .iter()
            .map(|diagnostic| (
                diagnostic.tool.as_str(),
                diagnostic.suppressed_source,
                diagnostic.winning_source,
            ))
            .collect::<Vec<_>>(),
        vec![
            ("grep", ToolSource::Builtin, ToolSource::ConfigDirectory),
            ("grep", ToolSource::ConfigDirectory, ToolSource::Plugin),
            ("grep", ToolSource::Plugin, ToolSource::Mcp),
        ]
    );
    assert_eq!(
        registry.diagnostics()[2].to_string(),
        "tool `grep` from plugin suppressed by same-named tool from MCP"
    );
}

#[test]
fn registry_appends_config_plugin_and_mcp_sources_in_that_order() {
    let root = TempDir::new().expect("temporary workspace");
    let files = FileTools::new(root.path()).expect("create file tools");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut builder = ToolRegistryBuilder::new(
        root.path(),
        Some(root.path().to_path_buf()),
        files,
        RegistryFlags::default(),
    );
    register_non_file_builtins(&mut builder);
    let registry = builder
        .with_custom_loader(Arc::new(FixedCustomLoader {
            seen_directories: Arc::clone(&seen),
        }))
        .with_mcp_loader(Arc::new(FixedMcpLoader))
        .build();

    let all = ids(registry.all());
    assert_eq!(
        &all[all.len() - 4..],
        ["config_default", "config_named", "plugin_tool", "mcp_tool"]
    );
    let expected = oc_paths::config_directories(root.path(), Some(root.path()));
    assert_eq!(registry.config_directories(), expected.as_slice());
    assert_eq!(*seen.lock().expect("read recorded directories"), expected);
}

#[test]
fn registry_rejects_wrong_ids_and_duplicate_slots() {
    let root = TempDir::new().expect("temporary workspace");
    let files = FileTools::new(root.path()).expect("create file tools");
    let mut builder = ToolRegistryBuilder::new(
        root.path(),
        Some(root.path().to_path_buf()),
        files,
        RegistryFlags::default(),
    );

    assert_eq!(
        builder
            .register_builtin(BuiltinSlot::Shell, stub("shell"))
            .err()
            .expect("shell's wire id is bash"),
        RegistryError::WrongBuiltinId {
            slot: BuiltinSlot::Shell,
            expected: "bash",
            actual: "shell".to_owned(),
        }
    );
    builder
        .register_builtin(BuiltinSlot::Shell, stub("bash"))
        .expect("first shell");
    assert_eq!(
        builder
            .register_builtin(BuiltinSlot::Shell, stub("bash"))
            .err()
            .expect("the slot is already occupied"),
        RegistryError::DuplicateBuiltin {
            slot: BuiltinSlot::Shell,
        }
    );
}

#[derive(Clone, Copy)]
struct DifferentialCase {
    label: &'static str,
    agent: &'static str,
    provider_id: &'static str,
    model_id: &'static str,
    permission: PermissionCase,
    enable_exa: bool,
    enable_lsp: bool,
    enable_plan: bool,
    expected: &'static [&'static str],
    expected_false: &'static [&'static str],
}

#[derive(Clone, Copy)]
enum PermissionCase {
    Default,
    DenyAllBash,
    DenyGitPush,
}

const DIFFERENTIAL_CASES: [DifferentialCase; 5] = [
    DifferentialCase {
        label: "gpt patch baseline",
        agent: "build",
        provider_id: "openai",
        model_id: "gpt-5.2",
        permission: PermissionCase::Default,
        enable_exa: false,
        enable_lsp: false,
        enable_plan: false,
        expected: &[
            "invalid",
            "question",
            "bash",
            "read",
            "glob",
            "grep",
            "task",
            "webfetch",
            "todowrite",
            "skill",
            "apply_patch",
        ],
        expected_false: &[],
    },
    DifferentialCase {
        label: "non-gpt with narrow bash deny",
        agent: "build",
        provider_id: "anthropic",
        model_id: "claude-sonnet-4-5",
        permission: PermissionCase::DenyGitPush,
        enable_exa: false,
        enable_lsp: false,
        enable_plan: false,
        expected: &[
            "invalid",
            "question",
            "bash",
            "read",
            "glob",
            "grep",
            "edit",
            "write",
            "task",
            "webfetch",
            "todowrite",
            "skill",
        ],
        expected_false: &[],
    },
    DifferentialCase {
        label: "gpt-4 carve-out with full bash deny",
        agent: "build",
        provider_id: "openai",
        model_id: "gpt-4.1",
        permission: PermissionCase::DenyAllBash,
        enable_exa: false,
        enable_lsp: false,
        enable_plan: false,
        expected: &[
            "invalid",
            "question",
            "read",
            "glob",
            "grep",
            "edit",
            "write",
            "task",
            "webfetch",
            "todowrite",
            "skill",
        ],
        expected_false: &["bash"],
    },
    DifferentialCase {
        label: "gpt-oss carve-out with search and lsp",
        agent: "build",
        provider_id: "openai",
        model_id: "gpt-oss-120b",
        permission: PermissionCase::Default,
        enable_exa: true,
        enable_lsp: true,
        enable_plan: false,
        expected: &[
            "invalid",
            "question",
            "bash",
            "read",
            "glob",
            "grep",
            "edit",
            "write",
            "task",
            "webfetch",
            "todowrite",
            "websearch",
            "skill",
            "lsp",
        ],
        expected_false: &[],
    },
    DifferentialCase {
        label: "hosted provider plan agent",
        agent: "plan",
        provider_id: "opencode",
        model_id: "gpt-5.2",
        permission: PermissionCase::Default,
        enable_exa: false,
        enable_lsp: false,
        enable_plan: true,
        expected: &[
            "invalid",
            "question",
            "bash",
            "read",
            "glob",
            "grep",
            "task",
            "webfetch",
            "todowrite",
            "websearch",
            "skill",
            "apply_patch",
            "plan_exit",
        ],
        expected_false: &[],
    },
];

fn rules(case: PermissionCase) -> Vec<Rule> {
    match case {
        PermissionCase::Default => Vec::new(),
        PermissionCase::DenyAllBash => vec![deny("bash", "*")],
        PermissionCase::DenyGitPush => vec![deny("bash", "git push*")],
    }
}

fn registry_flags(case: DifferentialCase) -> RegistryFlags {
    RegistryFlags {
        exposure: if case.enable_plan {
            ExposureFlags::default().with_plan_mode()
        } else {
            ExposureFlags::default()
        },
        search: SearchConfig {
            enable_exa: case.enable_exa,
            ..SearchConfig::default()
        },
        experimental_lsp_tool: case.enable_lsp,
        experimental_code_mode: false,
    }
}

fn expected_set(case: DifferentialCase) -> BTreeSet<String> {
    case.expected.iter().map(|id| (*id).to_owned()).collect()
}

fn permission_json(case: PermissionCase) -> Option<Value> {
    match case {
        PermissionCase::Default => None,
        PermissionCase::DenyAllBash => Some(json!({ "bash": "deny" })),
        PermissionCase::DenyGitPush => Some(json!({ "bash": { "git push*": "deny" } })),
    }
}

fn oracle_config(case: DifferentialCase) -> String {
    let mut agent = json!({
        "model": format!("{}/{}", case.provider_id, case.model_id),
    });
    if let Some(permission) = permission_json(case.permission) {
        agent["permission"] = permission;
    }
    let mut config = json!({ "agent": {} });
    config["agent"][case.agent] = agent;
    config.to_string()
}

fn run_oracle(binary: &Path, root: &Path, case: DifferentialCase) -> Value {
    std::fs::create_dir_all(root.join(".git")).expect("bound config discovery");
    let mut command = Command::new(binary);
    command
        .args(["debug", "agent", case.agent, "--pure"])
        .current_dir(root)
        .env_clear()
        .env("HOME", root.join("home"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "dumb")
        .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
        .env("OPENCODE_DISABLE_MODELS_FETCH", "1")
        .env("OPENCODE_CONFIG_CONTENT", oracle_config(case));
    if case.enable_exa {
        command.env("OPENCODE_ENABLE_EXA", "true");
    }
    if case.enable_lsp {
        command.env("OPENCODE_EXPERIMENTAL_LSP_TOOL", "true");
    }
    if case.enable_plan {
        command.env("OPENCODE_EXPERIMENTAL_PLAN_MODE", "true");
    }

    let output = command.output().expect("run real opencode");
    assert!(
        output.status.success(),
        "{} failed with {}\nstdout:\n{}\nstderr:\n{}",
        case.label,
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    serde_json::from_slice(&output.stdout).expect("debug agent emits JSON")
}

fn oracle_visible_set(output: &Value) -> BTreeSet<String> {
    output["tools"]
        .as_object()
        .expect("tools object")
        .iter()
        .filter(|(_, enabled)| enabled.as_bool() == Some(true))
        .map(|(id, _)| id.clone())
        .collect()
}

#[test]
fn registry_resolved_sets_match_five_real_binary_combinations() {
    assert_eq!(
        DIFFERENTIAL_CASES.len(),
        5,
        "the differential matrix is load-bearing"
    );
    let binary = pinned_oracle_or_skip(
        "registry_resolved_sets_match_five_real_binary_combinations",
        "the five captured tool sets were NOT compared against a real release",
    );

    for (index, case) in DIFFERENTIAL_CASES.into_iter().enumerate() {
        let workspace = TempDir::new().expect("temporary Rust workspace");
        let permission_rules = rules(case.permission);
        let subject = registry(workspace.path(), registry_flags(case));
        let subject_set: BTreeSet<String> = subject
            .resolved_ids(ResolveInput::new(
                case.model_id,
                case.provider_id,
                &permission_rules,
            ))
            .into_iter()
            .collect();
        let captured = expected_set(case);
        assert_eq!(subject_set, captured, "captured case: {}", case.label);

        let Some(binary) = binary else {
            return;
        };
        let oracle_root = TempDir::new().expect("temporary oracle workspace");
        let output = run_oracle(binary, oracle_root.path(), case);
        assert_eq!(
            oracle_visible_set(&output),
            captured,
            "real binary case {}: {}",
            index + 1,
            case.label,
        );
        for id in case.expected_false {
            assert_eq!(
                output["tools"][id],
                Value::Bool(false),
                "{} must retain {id}: false in debug output",
                case.label,
            );
        }
    }
}

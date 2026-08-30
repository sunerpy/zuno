use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;
use zuno_error::ToolError;
use zuno_permission::{PermissionAction, Rule};
use zuno_tool::{Tool, ToolContext, ToolOutput};
use zuno_tools::FileTools;
use zuno_tools::SearchConfig;
use zuno_tools::exposure::ExposureFlags;
use zuno_tools::registry::{
    BuiltinSlot, CustomTool, McpToolLoader, RegistryError, RegistryFlags, ResolveInput,
    TOOL_SOURCE_PRECEDENCE, ToolRegistry, ToolRegistryBuilder, ToolSource,
};

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
        (BuiltinSlot::Shell, "shell"),
        (BuiltinSlot::Background, "bg"),
        (BuiltinSlot::Glob, "glob"),
        (BuiltinSlot::Grep, "grep"),
        (BuiltinSlot::Task, "task"),
        (BuiltinSlot::Job, "job"),
        (BuiltinSlot::Fetch, "webfetch"),
        (BuiltinSlot::Search, "web_search"),
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
    let mut builder = ToolRegistryBuilder::new(root, files, flags);
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
fn registry_builtin_order_is_stable_before_turn_filters() {
    let root = TempDir::new().expect("temporary workspace");
    let registry = registry(root.path(), RegistryFlags::default());

    assert_eq!(
        ids(registry.all()),
        vec![
            "invalid",
            "question",
            "shell",
            "bg",
            "read",
            "glob",
            "grep",
            "edit",
            "write",
            "task",
            "job",
            "webfetch",
            "web_search",
            "skill",
            "apply_patch",
        ]
    );
}

#[test]
fn a_harness_manifest_filters_automatic_file_tools_and_registered_builtins_together() {
    let root = TempDir::new().expect("temporary workspace");
    let files = FileTools::new(root.path()).expect("create file tools");
    let mut builder = ToolRegistryBuilder::new(root.path(), files, RegistryFlags::default())
        .with_builtin_slots([BuiltinSlot::Read, BuiltinSlot::Task]);
    register_non_file_builtins(&mut builder);
    let registry = builder.build();

    assert_eq!(ids(registry.all()), ["read", "task"]);
}

#[test]
fn registry_file_surface_is_provider_neutral_and_has_a_write_fallback() {
    let root = TempDir::new().expect("temporary workspace");
    let registry = registry(root.path(), RegistryFlags::default());

    for model in ["claude-sonnet-4-5", "gpt-4.1", "gpt-oss-120b", "gpt-5.2"] {
        let offered = registry.resolved_ids(ResolveInput::new(model, "openai", &[]));
        assert!(offered.contains(&"write".to_owned()), "{model}");
        assert!(offered.contains(&"apply_patch".to_owned()), "{model}");
        assert!(!offered.contains(&"edit".to_owned()), "{model}");
    }
}

#[test]
fn registry_full_deny_hides_a_tool_but_a_narrow_deny_keeps_it_visible() {
    let root = TempDir::new().expect("temporary workspace");
    let registry = registry(root.path(), RegistryFlags::default());

    let fully_denied = registry.resolved_ids(ResolveInput::new(
        "claude-sonnet-4-5",
        "anthropic",
        &[deny("shell", "*")],
    ));
    assert!(!fully_denied.contains(&"shell".to_owned()));

    let narrowly_denied = registry.resolved_ids(ResolveInput::new(
        "claude-sonnet-4-5",
        "anthropic",
        &[deny("shell", "git push*")],
    ));
    assert!(narrowly_denied.contains(&"shell".to_owned()));
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

struct FixedMcpLoader;

impl McpToolLoader for FixedMcpLoader {
    fn tools(&self) -> Vec<CustomTool> {
        vec![stub("mcp_tool")]
    }
}

struct CollidingMcpLoader;

impl McpToolLoader for CollidingMcpLoader {
    fn tools(&self) -> Vec<CustomTool> {
        vec![tagged("grep", "MCP grep")]
    }
}

#[test]
fn registry_de_duplicates_cross_source_names_with_last_source_winning() {
    assert_eq!(
        TOOL_SOURCE_PRECEDENCE,
        [ToolSource::Builtin, ToolSource::Harness, ToolSource::Mcp],
        "the exported low-to-high precedence contract must stay pinned"
    );
    let root = TempDir::new().expect("temporary workspace");
    let files = FileTools::new(root.path()).expect("create file tools");
    let mut builder = ToolRegistryBuilder::new(root.path(), files, RegistryFlags::default());
    register_non_file_builtins(&mut builder);
    let registry = builder
        .with_harness_tools([tagged("grep", "harness grep")])
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
            ("grep", ToolSource::Builtin, ToolSource::Harness),
            ("grep", ToolSource::Harness, ToolSource::Mcp),
        ]
    );
    assert_eq!(
        registry.diagnostics()[1].to_string(),
        "tool `grep` from harness suppressed by same-named tool from MCP"
    );
}

#[test]
fn registry_appends_harness_and_mcp_sources_in_that_order() {
    let root = TempDir::new().expect("temporary workspace");
    let files = FileTools::new(root.path()).expect("create file tools");
    let mut builder = ToolRegistryBuilder::new(root.path(), files, RegistryFlags::default());
    register_non_file_builtins(&mut builder);
    let registry = builder
        .with_harness_tools([stub("harness_default"), stub("harness_named")])
        .with_mcp_loader(Arc::new(FixedMcpLoader))
        .build();

    let all = ids(registry.all());
    assert_eq!(
        &all[all.len() - 3..],
        ["harness_default", "harness_named", "mcp_tool"]
    );
}

#[test]
fn registry_rejects_wrong_ids_and_duplicate_slots() {
    let root = TempDir::new().expect("temporary workspace");
    let files = FileTools::new(root.path()).expect("create file tools");
    let mut builder = ToolRegistryBuilder::new(root.path(), files, RegistryFlags::default());

    assert_eq!(
        builder
            .register_builtin(BuiltinSlot::Shell, stub("bash"))
            .err()
            .expect("the historical bash id is rejected"),
        RegistryError::WrongBuiltinId {
            slot: BuiltinSlot::Shell,
            expected: "shell",
            actual: "bash".to_owned(),
        }
    );
    builder
        .register_builtin(BuiltinSlot::Shell, stub("shell"))
        .expect("first shell");
    assert_eq!(
        builder
            .register_builtin(BuiltinSlot::Shell, stub("shell"))
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
    provider_id: &'static str,
    model_id: &'static str,
    permission: PermissionCase,
    enable_exa: bool,
    enable_lsp: bool,
    enable_plan: bool,
    expected: &'static [&'static str],
}

#[derive(Clone, Copy)]
enum PermissionCase {
    Default,
    DenyAllShell,
    DenyGitPush,
}

const DIFFERENTIAL_CASES: [DifferentialCase; 5] = [
    DifferentialCase {
        label: "gpt patch baseline",
        provider_id: "openai",
        model_id: "gpt-5.2",
        permission: PermissionCase::Default,
        enable_exa: false,
        enable_lsp: false,
        enable_plan: false,
        expected: &[
            "invalid",
            "question",
            "shell",
            "bg",
            "read",
            "glob",
            "grep",
            "write",
            "task",
            "job",
            "webfetch",
            "skill",
            "apply_patch",
        ],
    },
    DifferentialCase {
        label: "non-gpt with narrow shell deny",
        provider_id: "anthropic",
        model_id: "claude-sonnet-4-5",
        permission: PermissionCase::DenyGitPush,
        enable_exa: false,
        enable_lsp: false,
        enable_plan: false,
        expected: &[
            "invalid",
            "question",
            "shell",
            "bg",
            "read",
            "glob",
            "grep",
            "write",
            "task",
            "job",
            "webfetch",
            "skill",
            "apply_patch",
        ],
    },
    DifferentialCase {
        label: "gpt-4 carve-out with full shell deny",
        provider_id: "openai",
        model_id: "gpt-4.1",
        permission: PermissionCase::DenyAllShell,
        enable_exa: false,
        enable_lsp: false,
        enable_plan: false,
        expected: &[
            "invalid",
            "question",
            "bg",
            "read",
            "glob",
            "grep",
            "write",
            "task",
            "job",
            "webfetch",
            "skill",
            "apply_patch",
        ],
    },
    DifferentialCase {
        label: "gpt-oss carve-out with search and lsp",
        provider_id: "openai",
        model_id: "gpt-oss-120b",
        permission: PermissionCase::Default,
        enable_exa: true,
        enable_lsp: true,
        enable_plan: false,
        expected: &[
            "invalid",
            "question",
            "shell",
            "bg",
            "read",
            "glob",
            "grep",
            "write",
            "task",
            "job",
            "webfetch",
            "web_search",
            "skill",
            "apply_patch",
            "lsp",
        ],
    },
    DifferentialCase {
        label: "plan agent with native search",
        provider_id: "openai",
        model_id: "gpt-5.2",
        permission: PermissionCase::Default,
        enable_exa: true,
        enable_lsp: false,
        enable_plan: true,
        expected: &[
            "invalid",
            "question",
            "shell",
            "bg",
            "read",
            "glob",
            "grep",
            "write",
            "task",
            "job",
            "webfetch",
            "web_search",
            "skill",
            "apply_patch",
            "plan_exit",
        ],
    },
];

fn rules(case: PermissionCase) -> Vec<Rule> {
    match case {
        PermissionCase::Default => Vec::new(),
        PermissionCase::DenyAllShell => vec![deny("shell", "*")],
        PermissionCase::DenyGitPush => vec![deny("shell", "git push*")],
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
            enabled: case.enable_exa,
            ..SearchConfig::default()
        },
        experimental_lsp_tool: case.enable_lsp,
        experimental_code_mode: false,
    }
}

fn expected_set(case: DifferentialCase) -> BTreeSet<String> {
    case.expected.iter().map(|id| (*id).to_owned()).collect()
}

#[test]
fn registry_resolved_sets_match_five_native_compositions() {
    assert_eq!(
        DIFFERENTIAL_CASES.len(),
        5,
        "the differential matrix is load-bearing"
    );
    for case in DIFFERENTIAL_CASES {
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
    }
}

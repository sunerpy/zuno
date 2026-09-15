use super::*;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

fn source(scope: WorkflowSourceScope, id: &str) -> WorkflowSource {
    WorkflowSource::new(scope, id).unwrap()
}

fn graph(name: &str, marker: &str) -> String {
    format!(
        r#"apiVersion: zuno.workflow/v1
kind: Workflow
metadata:
  name: {name}
  version: "1"
  description: {marker}
spec:
  engine: graph/v1
  routes:
    work: {{ agentRef: worker }}
  nodes:
    - {{ id: work, route: work }}
"#
    )
}

fn script_file(name: &str) -> String {
    format!(
        r#"apiVersion: zuno.workflow/v1
kind: Workflow
metadata: {{ name: {name}, version: "1" }}
spec:
  engine: javascript/v1
  scriptFile: scripts/run.js
"#
    )
}

#[test]
fn empty_registry_has_no_application_owned_workflows() {
    let outcome = load_local_workflow_registry([], LocalWorkflowLoadLimits::default());
    assert!(outcome.registry.is_empty());
    assert!(outcome.registry.get("frontend-consensus").is_none());
    assert!(outcome.diagnostics.is_empty());
}

#[test]
fn user_may_define_frontend_consensus_without_an_application_special_case() {
    let fixture = TempDir::new().unwrap();
    fs::write(
        fixture.path().join("frontend-consensus.yaml"),
        graph("frontend-consensus", "user-owned"),
    )
    .unwrap();

    let outcome = load_local_workflow_registry(
        [LocalWorkflowRoot::required(
            source(WorkflowSourceScope::User, "home"),
            fixture.path(),
        )],
        LocalWorkflowLoadLimits::default(),
    );

    let workflow = outcome
        .registry
        .get("frontend-consensus")
        .expect("user-authored workflow should be discoverable");
    assert_eq!(workflow.source().scope, WorkflowSourceScope::User);
    assert_eq!(
        workflow
            .workflow()
            .definition()
            .metadata
            .description
            .as_deref(),
        Some("user-owned")
    );
    assert!(outcome.diagnostics.is_empty());
}

#[test]
fn project_overrides_user_and_plugin_without_hiding_provenance() {
    let fixture = TempDir::new().unwrap();
    let plugin = fixture.path().join("plugin");
    let user = fixture.path().join("user");
    let project = fixture.path().join("project");
    for (directory, marker) in [(&plugin, "plugin"), (&user, "user"), (&project, "project")] {
        fs::create_dir_all(directory).unwrap();
        fs::write(directory.join("review.yaml"), graph("review", marker)).unwrap();
    }

    let outcome = load_local_workflow_registry(
        [
            LocalWorkflowRoot::required(source(WorkflowSourceScope::Project, "repo"), &project),
            LocalWorkflowRoot::required(source(WorkflowSourceScope::Plugin, "kit"), &plugin),
            LocalWorkflowRoot::required(source(WorkflowSourceScope::User, "home"), &user),
        ],
        LocalWorkflowLoadLimits::default(),
    );

    let active = outcome.registry.get("review").unwrap();
    assert_eq!(active.source().scope, WorkflowSourceScope::Project);
    assert_eq!(
        active
            .workflow()
            .definition()
            .metadata
            .description
            .as_deref(),
        Some("project")
    );
    assert_eq!(outcome.registry.shadowed().len(), 2);
    assert!(outcome.diagnostics.is_empty());
}

#[test]
fn same_scope_duplicate_names_fail_closed() {
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("a.yaml"), graph("review", "a")).unwrap();
    fs::write(fixture.path().join("b.yaml"), graph("review", "b")).unwrap();

    let outcome = load_local_workflow_registry(
        [LocalWorkflowRoot::required(
            source(WorkflowSourceScope::User, "home"),
            fixture.path(),
        )],
        LocalWorkflowLoadLimits::default(),
    );

    assert!(outcome.registry.get("review").is_none());
    assert_eq!(outcome.registry.shadowed().len(), 2);
    assert_eq!(outcome.diagnostics.len(), 1);
    assert_eq!(
        outcome.diagnostics[0].code,
        WorkflowDiagnosticCode::DuplicateName
    );
}

#[test]
fn discovery_budgets_preserve_higher_precedence_sources() {
    let fixture = TempDir::new().unwrap();
    let plugin = fixture.path().join("plugin");
    let project = fixture.path().join("project");
    fs::create_dir_all(&plugin).unwrap();
    fs::create_dir_all(&project).unwrap();
    fs::write(plugin.join("plugin.yaml"), graph("plugin-only", "plugin")).unwrap();
    fs::write(
        project.join("project.yaml"),
        graph("project-only", "project"),
    )
    .unwrap();
    let roots = || {
        [
            LocalWorkflowRoot::required(source(WorkflowSourceScope::Plugin, "kit"), &plugin),
            LocalWorkflowRoot::required(source(WorkflowSourceScope::Project, "repo"), &project),
        ]
    };

    let root_limited = load_local_workflow_registry(
        roots(),
        LocalWorkflowLoadLimits {
            max_roots: 1,
            ..LocalWorkflowLoadLimits::default()
        },
    );
    assert!(root_limited.registry.get("project-only").is_some());
    assert!(root_limited.registry.get("plugin-only").is_none());

    let file_limited = load_local_workflow_registry(
        roots(),
        LocalWorkflowLoadLimits {
            max_files: 1,
            ..LocalWorkflowLoadLimits::default()
        },
    );
    assert!(file_limited.registry.get("project-only").is_some());
    assert!(file_limited.registry.get("plugin-only").is_none());
}

#[test]
fn duplicate_root_declarations_are_loaded_once() {
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("flow.yaml"), graph("flow", "one")).unwrap();
    let root =
        LocalWorkflowRoot::optional(source(WorkflowSourceScope::User, "home"), fixture.path());
    let mut required = root.clone();
    required.required = true;

    let outcome = load_local_workflow_registry(
        [root, required],
        LocalWorkflowLoadLimits {
            max_roots: 1,
            ..LocalWorkflowLoadLimits::default()
        },
    );

    assert!(outcome.registry.get("flow").is_some());
    assert!(outcome.diagnostics.is_empty());
}

#[test]
fn higher_precedence_workflow_survives_a_shadowed_layer_conflict() {
    let fixture = TempDir::new().unwrap();
    let user = fixture.path().join("user");
    let project = fixture.path().join("project");
    fs::create_dir_all(&user).unwrap();
    fs::create_dir_all(&project).unwrap();
    fs::write(user.join("a.yaml"), graph("review", "user-a")).unwrap();
    fs::write(user.join("b.yaml"), graph("review", "user-b")).unwrap();
    fs::write(project.join("review.yaml"), graph("review", "project")).unwrap();

    let outcome = load_local_workflow_registry(
        [
            LocalWorkflowRoot::required(source(WorkflowSourceScope::User, "home"), &user),
            LocalWorkflowRoot::required(source(WorkflowSourceScope::Project, "repo"), &project),
        ],
        LocalWorkflowLoadLimits::default(),
    );

    assert_eq!(
        outcome.registry.get("review").unwrap().source().scope,
        WorkflowSourceScope::Project
    );
    assert_eq!(outcome.registry.shadowed().len(), 2);
    assert_eq!(outcome.diagnostics.len(), 1);
    assert_eq!(
        outcome.diagnostics[0].code,
        WorkflowDiagnosticCode::DuplicateName
    );
}

#[test]
fn one_physical_document_can_be_owned_by_distinct_layers() {
    let fixture = TempDir::new().unwrap();
    let file = fixture.path().join("shared.yaml");
    fs::write(&file, graph("shared", "shared")).unwrap();

    let outcome = load_local_workflow_registry(
        [
            LocalWorkflowRoot::required(source(WorkflowSourceScope::Plugin, "kit"), &file),
            LocalWorkflowRoot::required(source(WorkflowSourceScope::Project, "repo"), &file),
        ],
        LocalWorkflowLoadLimits::default(),
    );

    assert_eq!(
        outcome.registry.get("shared").unwrap().source().scope,
        WorkflowSourceScope::Project
    );
    assert_eq!(outcome.registry.shadowed().len(), 1);
}

#[test]
fn resolves_script_file_and_binds_it_into_executable_digest() {
    let fixture = TempDir::new().unwrap();
    fs::create_dir_all(fixture.path().join("scripts")).unwrap();
    fs::write(fixture.path().join("flow.yaml"), script_file("scripted")).unwrap();
    fs::write(fixture.path().join("scripts/run.js"), "return args.first;").unwrap();
    let root =
        LocalWorkflowRoot::required(source(WorkflowSourceScope::Project, "repo"), fixture.path());

    let first = load_local_workflow_registry([root.clone()], LocalWorkflowLoadLimits::default());
    let first = first.registry.get("scripted").unwrap();
    assert_eq!(
        first.compile_request().resolved_script.as_deref(),
        Some("return args.first;")
    );
    let first_digest = first.executable_digest().to_string();

    fs::write(fixture.path().join("scripts/run.js"), "return args.second;").unwrap();
    let second = load_local_workflow_registry([root], LocalWorkflowLoadLimits::default());
    assert_ne!(
        first_digest,
        second.registry.get("scripted").unwrap().executable_digest()
    );
}

#[test]
fn rejects_symlinked_documents_and_scripts() {
    let fixture = TempDir::new().unwrap();
    let root = fixture.path().join("flows");
    fs::create_dir_all(root.join("scripts")).unwrap();
    fs::write(root.join("flow.yaml"), script_file("scripted")).unwrap();
    let outside = fixture.path().join("outside.js");
    fs::write(&outside, "return 1;").unwrap();

    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, root.join("scripts/run.js")).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&outside, root.join("scripts/run.js")).unwrap();

    let outcome = load_local_workflow_registry(
        [LocalWorkflowRoot::required(
            source(WorkflowSourceScope::Plugin, "unsafe"),
            &root,
        )],
        LocalWorkflowLoadLimits::default(),
    );
    assert!(outcome.registry.is_empty());
    assert!(
        outcome
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == WorkflowDiagnosticCode::SymlinkRejected)
    );
}

#[test]
fn document_and_script_byte_limits_fail_closed() {
    let fixture = TempDir::new().unwrap();
    fs::create_dir_all(fixture.path().join("scripts")).unwrap();
    fs::write(fixture.path().join("graph.yaml"), graph("graph", "large")).unwrap();
    fs::write(fixture.path().join("script.yaml"), script_file("scripted")).unwrap();
    fs::write(fixture.path().join("scripts/run.js"), "return args;").unwrap();
    let root =
        LocalWorkflowRoot::required(source(WorkflowSourceScope::Project, "repo"), fixture.path());

    let document_limited = load_local_workflow_registry(
        [root.clone()],
        LocalWorkflowLoadLimits {
            max_document_bytes: 8,
            ..LocalWorkflowLoadLimits::default()
        },
    );
    assert!(document_limited.registry.is_empty());
    assert!(
        document_limited
            .diagnostics
            .iter()
            .all(|diagnostic| { diagnostic.code == WorkflowDiagnosticCode::DocumentTooLarge })
    );

    let script_limited = load_local_workflow_registry(
        [root],
        LocalWorkflowLoadLimits {
            max_script_bytes: 4,
            ..LocalWorkflowLoadLimits::default()
        },
    );
    assert!(script_limited.registry.get("graph").is_some());
    assert!(script_limited.registry.get("scripted").is_none());
    assert!(
        script_limited
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.code == WorkflowDiagnosticCode::ScriptTooLarge })
    );
}

#[test]
fn malformed_document_does_not_block_an_independent_workflow() {
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("bad.yaml"), "not: a workflow").unwrap();
    fs::write(fixture.path().join("good.yml"), graph("good", "ok")).unwrap();

    let outcome = load_local_workflow_registry(
        [LocalWorkflowRoot::required(
            source(WorkflowSourceScope::User, "home"),
            fixture.path(),
        )],
        LocalWorkflowLoadLimits::default(),
    );
    assert!(outcome.registry.get("good").is_some());
    assert_eq!(outcome.diagnostics.len(), 1);
    assert_eq!(outcome.diagnostics[0].code, WorkflowDiagnosticCode::Parse);
}

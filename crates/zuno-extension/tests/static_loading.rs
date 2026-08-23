use std::fs;
use std::path::Path;

use serde_json::json;
use tempfile::tempdir;
use zuno_config::schema::permission::{PermissionAction, PermissionRule};
use zuno_extension::{
    API_VERSION, ExtensionRegistry, Package, Scope, StaticPackage, discover_static, resolve_active,
};
use zuno_paths::Env;

fn write_package(root: &Path, directory_name: &str, id: &str) {
    let directory = root
        .join(zuno_paths::PROJECT_DIRECTORY)
        .join("extensions")
        .join(directory_name);
    fs::create_dir_all(&directory).expect("extension directory");
    fs::write(
        directory.join("extension.json"),
        serde_json::to_vec_pretty(&json!({
            "apiVersion": API_VERSION,
            "id": id,
            "description": "A persistent package",
            "workflows": {
                "persisted": {
                    "description": "A persisted workflow",
                    "prompt": "Run the persisted workflow."
                }
            }
        }))
        .expect("json"),
    )
    .expect("manifest");
}

#[test]
fn static_packages_are_discovered_and_active_on_every_load() {
    let workspace = tempdir().expect("workspace");
    write_package(workspace.path(), "persistent", "persistent");
    let env = Env::empty().with("HOME", workspace.path().join("home").to_string_lossy());

    let first = discover_static(workspace.path(), Some(workspace.path()), &env)
        .expect("first static discovery");
    let second = discover_static(workspace.path(), Some(workspace.path()), &env)
        .expect("second static discovery");

    assert_eq!(first, second);
    let active = resolve_active(
        &Scope::new(workspace.path()),
        &first,
        &ExtensionRegistry::new(),
    )
    .expect("static package resolves");
    assert_eq!(active.packages().len(), 1);
    assert!(active.workflows().contains_key("persisted"));
}

#[test]
fn a_static_directory_and_package_id_must_agree() {
    let workspace = tempdir().expect("workspace");
    write_package(workspace.path(), "directory-name", "different-id");
    let env = Env::empty().with("HOME", workspace.path().join("home").to_string_lossy());

    let error = discover_static(workspace.path(), Some(workspace.path()), &env)
        .expect_err("mismatched package provenance must fail");

    assert!(error.to_string().contains("directory-name"));
    assert!(error.to_string().contains("different-id"));
}

#[test]
fn duplicate_contribution_names_fail_instead_of_silently_shadowing() {
    let first: Package = serde_json::from_value(json!({
        "apiVersion": API_VERSION,
        "id": "first",
        "description": "first",
        "agents": {
            "same": { "prompt": "first" }
        }
    }))
    .expect("first package");
    let second: Package = serde_json::from_value(json!({
        "apiVersion": API_VERSION,
        "id": "second",
        "description": "second",
        "agents": {
            "same": { "prompt": "second" }
        }
    }))
    .expect("second package");
    let first = StaticPackage::new(
        first,
        Path::new("/repo/.zuno/extensions/first/extension.json"),
    )
    .expect("matching first provenance");
    let second = StaticPackage::new(
        second,
        Path::new("/repo/.zuno/extensions/second/extension.json"),
    )
    .expect("matching second provenance");

    let error = resolve_active(
        &Scope::new(Path::new("/repo")),
        &[first, second],
        &ExtensionRegistry::new(),
    )
    .expect_err("two active packages cannot claim one agent");

    assert!(error.to_string().contains("agent"));
    assert!(error.to_string().contains("same"));
}

#[test]
fn extension_agents_keep_native_file_network_and_environment_tool_permissions() {
    let package: Package = serde_json::from_value(json!({
        "apiVersion": API_VERSION,
        "id": "capable-agent",
        "description": "custom agent capability fixture",
        "agents": {
            "network-reviewer": {
                "mode": "subagent",
                "prompt": "Inspect files, network evidence, and environment facts.",
                "permission": {
                    "read": "allow",
                    "web_search": "allow",
                    "bash": "ask"
                }
            }
        }
    }))
    .expect("valid extension agent");
    let package = StaticPackage::new(
        package,
        Path::new("/repo/.zuno/extensions/capable-agent/extension.json"),
    )
    .expect("matching package provenance");
    let resolved = resolve_active(
        &Scope::new(Path::new("/repo")),
        &[package],
        &ExtensionRegistry::new(),
    )
    .expect("package resolves");
    let permission = resolved
        .agents()
        .get("network-reviewer")
        .and_then(|agent| agent.permission.as_ref())
        .expect("agent permission survives extension resolution")
        .normalized();

    for (tool, expected) in [
        ("read", PermissionAction::Allow),
        ("web_search", PermissionAction::Allow),
        ("bash", PermissionAction::Ask),
    ] {
        assert_eq!(
            permission.get(tool),
            Some(&PermissionRule::Action(expected)),
            "{tool} capability changed"
        );
    }
}

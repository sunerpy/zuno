use serde_json::json;
use zuno_extension::{API_VERSION, ExtensionRegistry, Package, Scope};

#[test]
fn an_extension_agent_cannot_rename_its_map_identity() {
    let error = serde_json::from_value::<Package>(json!({
        "apiVersion": API_VERSION,
        "id": "unsafe-rename",
        "description": "must not bypass contribution collision checks",
        "agents": {
            "reviewer": {
                "name": "build",
                "prompt": "Replace the active build agent."
            }
        }
    }))
    .expect_err("an agent contribution must keep the map key as its identity");

    assert!(error.to_string().contains("reviewer"));
    assert!(error.to_string().contains("rename"));
}

#[test]
fn an_extension_agent_must_contribute_instead_of_disabling_itself() {
    let error = serde_json::from_value::<Package>(json!({
        "apiVersion": API_VERSION,
        "id": "disabled-agent",
        "description": "must contribute a real agent",
        "agents": {
            "reviewer": {
                "disable": true
            }
        }
    }))
    .expect_err("a disabled entry is not an agent contribution");

    assert!(error.to_string().contains("reviewer"));
    assert!(error.to_string().contains("disabled"));
}

#[test]
fn executable_tools_require_an_explicit_runtime() {
    let error = serde_json::from_value::<Package>(json!({
        "apiVersion": API_VERSION,
        "id": "missing-runtime",
        "description": "invalid executable package",
        "tools": {
            "example": {
                "description": "Example tool"
            }
        }
    }))
    .expect_err("a tool proxy without a host cannot be published");

    assert!(error.to_string().contains("without a runtime"));
}

#[test]
fn executable_runtimes_require_a_consumed_tool_interface() {
    let error = serde_json::from_value::<Package>(json!({
        "apiVersion": API_VERSION,
        "id": "orphan-runtime",
        "description": "must not execute code without a tool consumer",
        "runtime": {
            "kind": "process",
            "command": "plugin",
            "capabilities": ["host.full"]
        }
    }))
    .expect_err("a runtime without tools has no complete capability");

    assert!(error.to_string().contains("without any tools"));
}

#[test]
fn process_plugins_must_admit_their_full_host_authority() {
    let error = serde_json::from_value::<Package>(json!({
        "apiVersion": API_VERSION,
        "id": "dishonest-process",
        "description": "invalid process capability declaration",
        "runtime": {
            "kind": "process",
            "command": "plugin",
            "capabilities": ["workspace.read"]
        },
        "tools": {
            "inspect": {
                "description": "Inspect one subject"
            }
        }
    }))
    .expect_err("an OS process cannot enforce a workspace-only grant");

    assert!(error.to_string().contains("host.full"));
}

#[test]
fn mutation_capable_runtimes_cannot_claim_read_only_or_safe_replay() {
    for (id, runtime) in [
        (
            "host-full-read",
            json!({
                "kind": "process",
                "command": "plugin",
                "capabilities": ["host.full"]
            }),
        ),
        (
            "network-read",
            json!({
                "kind": "wasi",
                "artifact": "plugin.wasm",
                "capabilities": ["network"]
            }),
        ),
        (
            "workspace-write-read",
            json!({
                "kind": "wasi",
                "artifact": "plugin.wasm",
                "capabilities": ["workspace.write"]
            }),
        ),
    ] {
        let error = serde_json::from_value::<Package>(json!({
            "apiVersion": API_VERSION,
            "id": id,
            "description": "must fail closed",
            "runtime": runtime,
            "tools": {
                "inspect": {
                    "description": "Claims an unenforceable read-only effect",
                    "effect": "readOnly",
                    "replay": "safe"
                }
            }
        }))
        .expect_err("mutation-capable authority cannot bypass strict HITL");

        assert!(error.to_string().contains("cannot enforce"));
        assert!(error.to_string().contains("sideEffecting"));
    }

    serde_json::from_value::<Package>(json!({
        "apiVersion": API_VERSION,
        "id": "bounded-read",
        "description": "read-only capability can be enforced",
        "runtime": {
            "kind": "wasi",
            "artifact": "plugin.wasm",
            "capabilities": ["workspace.read"]
        },
        "tools": {
            "inspect": {
                "description": "Inspect the workspace",
                "effect": "readOnly",
                "replay": "safe"
            }
        }
    }))
    .expect("a WASI guest with no mutation grant may declare read-only");
}

#[test]
fn safe_replay_is_reserved_for_read_only_tools() {
    let error = serde_json::from_value::<Package>(json!({
        "apiVersion": API_VERSION,
        "id": "unsafe-replay",
        "description": "invalid replay declaration",
        "runtime": {
            "kind": "process",
            "command": "plugin",
            "capabilities": ["host.full"]
        },
        "tools": {
            "mutate": {
                "description": "Mutate something",
                "replay": "safe"
            }
        }
    }))
    .expect_err("side effects cannot opt into mechanical replay");

    assert!(error.to_string().contains("readOnly"));
}

#[test]
fn wasi_memory_budget_uses_the_explicit_mib_unit_spelling() {
    serde_json::from_value::<Package>(json!({
        "apiVersion": API_VERSION,
        "id": "wasi-budget",
        "description": "valid WASI budget",
        "runtime": {
            "kind": "wasi",
            "artifact": "plugin.wasm",
            "memoryMiB": 96
        },
        "tools": {
            "inspect": {
                "description": "Inspect bounded input"
            }
        }
    }))
    .expect("memoryMiB is the canonical field");

    let error = serde_json::from_value::<Package>(json!({
        "apiVersion": API_VERSION,
        "id": "wasi-budget",
        "description": "invalid automatic casing",
        "runtime": {
            "kind": "wasi",
            "artifact": "plugin.wasm",
            "memoryMib": 96
        },
        "tools": {
            "inspect": {
                "description": "Inspect bounded input"
            }
        }
    }))
    .expect_err("the unreleased schema has one spelling and no compatibility alias");
    assert!(error.to_string().contains("memoryMib"));
    assert!(error.to_string().contains("memoryMiB"));
}

#[test]
fn process_local_definitions_cannot_smuggle_executable_code() {
    let package = serde_json::from_value::<Package>(json!({
        "apiVersion": API_VERSION,
        "id": "dynamic-runtime",
        "description": "must be installed statically",
        "runtime": {
            "kind": "process",
            "command": "plugin",
            "capabilities": ["host.full"]
        },
        "tools": {
            "mutate": {
                "description": "Execute installed runtime code"
            }
        }
    }))
    .expect("the manifest is valid for static installation");
    let registry = ExtensionRegistry::new();
    let error = registry
        .define(&Scope::new(std::path::Path::new("/repo")), package)
        .expect_err("model-authored definitions cannot launch executable code");

    assert!(error.to_string().contains("static package"));
}

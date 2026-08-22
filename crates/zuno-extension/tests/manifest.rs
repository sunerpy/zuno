use serde_json::json;
use zuno_extension::{API_VERSION, Package};

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

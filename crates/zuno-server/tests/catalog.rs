//! Agent catalog projection: canonical role definitions and user override order.
//! Keep these checks independent of session requests and interaction handlers.

use std::path::{Path, PathBuf};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;
use zuno_catalog::agent::{self, builtin};
use zuno_paths::Env;
use zuno_permission::visibility::permission_key;
use zuno_permission::{PermissionAction, Rule, evaluate, rules_from_config};
use zuno_server::api::{self, ApiState};
use zuno_server::{ServerBuilder, ServerConfig};

struct CatalogFixture {
    _root: TempDir,
    directory: PathBuf,
    env: Env,
}

impl CatalogFixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("catalog fixture");
        let directory = root.path().join("workspace");
        std::fs::create_dir_all(&directory).expect("workspace");
        let env = Env::empty()
            .with("HOME", path_string(&root.path().join("home")))
            .with("XDG_CONFIG_HOME", path_string(&root.path().join("config")))
            .with("XDG_DATA_HOME", path_string(&root.path().join("data")))
            .with("XDG_CACHE_HOME", path_string(&root.path().join("cache")))
            .with("XDG_STATE_HOME", path_string(&root.path().join("state")));
        Self {
            _root: root,
            directory,
            env,
        }
    }

    fn configure(&mut self, config: Value) {
        self.env = self
            .env
            .clone()
            .with("ZUNO_CONFIG_CONTENT", config.to_string());
    }

    async fn agents(&self) -> Vec<Value> {
        let state = ApiState::memory(path_string(&self.directory))
            .expect("API state")
            .with_env(self.env.clone());
        let app = ServerBuilder::new(
            ServerConfig::default().with_default_directory(path_string(&self.directory)),
        )
        .with_routes(api::router(state))
        .router();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/agent")
                    .body(Body::empty())
                    .expect("catalog request"),
            )
            .await
            .expect("catalog response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .expect("bounded catalog body");
        let envelope: Value = serde_json::from_slice(&body).expect("catalog JSON");
        assert_eq!(
            envelope["location"]["directory"],
            path_string(&self.directory)
        );
        envelope["data"].as_array().expect("agent array").clone()
    }
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn by_name<'a>(agents: &'a [Value], name: &str) -> &'a Value {
    agents
        .iter()
        .find(|agent| agent["id"] == name)
        .unwrap_or_else(|| panic!("missing Agent {name}"))
}

fn permission_rules(agent: &Value) -> Vec<Rule> {
    agent["permissions"]
        .as_array()
        .expect("permission rules")
        .iter()
        .map(|rule| Rule {
            source: None,
            permission: rule["action"].as_str().expect("permission key").to_owned(),
            pattern: rule["resource"]
                .as_str()
                .expect("resource pattern")
                .to_owned(),
            action: match rule["effect"].as_str().expect("effect") {
                "allow" => PermissionAction::Allow,
                "ask" => PermissionAction::Ask,
                "deny" => PermissionAction::Deny,
                effect => panic!("unknown permission effect {effect}"),
            },
        })
        .collect()
}

#[tokio::test]
async fn api_agent_catalog_uses_canonical_order_prompts_and_role_permissions() {
    let fixture = CatalogFixture::new();
    let expected = agent::load(&fixture.directory, None, &fixture.env).expect("canonical catalog");
    let actual = fixture.agents().await;
    assert_eq!(actual.len(), 15, "all native roles must remain present");
    assert_eq!(
        actual
            .iter()
            .map(|agent| agent["id"].as_str().expect("id"))
            .collect::<Vec<_>>(),
        expected
            .iter()
            .map(|agent| agent.name.as_str())
            .collect::<Vec<_>>(),
        "HTTP must preserve canonical catalog ordering"
    );

    for definition in expected {
        let projected = by_name(&actual, &definition.name);
        assert_eq!(projected["system"].as_str(), definition.prompt.as_deref());
        assert_eq!(
            projected["description"].as_str(),
            definition.description.as_deref()
        );
        assert_eq!(projected["mode"], agent::mode_label(definition.mode));
        assert_eq!(projected["hidden"], definition.hidden.unwrap_or(false));
        let native = builtin::get(&definition.name).expect("native definition");
        let overlay = rules_from_config(&native.permission_overlay().expect("native policy"));
        let projected_rules = permission_rules(projected);
        assert!(
            projected_rules
                .windows(overlay.len())
                .any(|window| window == overlay),
            "{} must include the canonical role overlay without substitutions",
            definition.name
        );
        let mut expected_rules = vec![Rule {
            source: None,
            permission: "*".to_owned(),
            pattern: "*".to_owned(),
            action: PermissionAction::Allow,
        }];
        expected_rules.extend(overlay);
        for tool in [
            "read",
            "shell",
            "bg",
            "edit",
            "write",
            "apply_patch",
            "execute",
            "task",
            "job",
            "web_search",
            "goal_get",
            "goal_update",
            "plan_update",
            "todo_update",
            "skill",
            "tool_search",
            "report_write",
            "memory_update",
            "review_open",
            "review_finalize",
            "council_run",
            "unknown_tool",
        ] {
            // These resources do not use the common path-specific default rules.
            assert_eq!(
                evaluate(permission_key(tool), "catalog-probe", &projected_rules),
                evaluate(permission_key(tool), "catalog-probe", &expected_rules),
                "{}: HTTP disagrees with native role policy for {tool}",
                definition.name
            );
        }
    }

    let plan = permission_rules(by_name(&actual, "plan"));
    assert_eq!(
        evaluate("edit", ".zuno/plans/example.md", &plan),
        PermissionAction::Deny,
        "the catalog must not invent a plan-file edit grant"
    );
    let review = permission_rules(by_name(&actual, "review"));
    assert_eq!(evaluate("task", "*", &review), PermissionAction::Deny);
    for name in ["compaction", "title", "summary", "council-synth"] {
        let internal = by_name(&actual, name);
        assert_eq!(internal["hidden"], true);
        assert_eq!(
            evaluate("unknown_tool", "*", &permission_rules(internal)),
            PermissionAction::Deny,
            "{name} must remain tool-free"
        );
    }
}

#[tokio::test]
async fn api_agent_catalog_preserves_every_native_prompt_override_including_empty() {
    let mut fixture = CatalogFixture::new();
    let overrides = builtin::BUILTIN_NAMES
        .into_iter()
        .map(|name| {
            let prompt = if matches!(name, "build" | "title") {
                String::new()
            } else {
                format!("User-owned {name} instructions.\n")
            };
            (name.to_owned(), json!({ "prompt": prompt }))
        })
        .collect::<serde_json::Map<_, _>>();
    fixture.configure(json!({ "agents": overrides }));
    let actual = fixture.agents().await;
    for name in builtin::BUILTIN_NAMES {
        assert_eq!(
            by_name(&actual, name)["system"],
            overrides[name]["prompt"],
            "{name}: an explicit prompt must survive the HTTP projection verbatim"
        );
    }
}

#[tokio::test]
async fn api_agent_catalog_applies_global_then_agent_rules_and_keeps_custom_roles() {
    let mut fixture = CatalogFixture::new();
    fixture.configure(json!({
        "permission": {
            "rules": { "shell": "deny", "web_search": "deny" }
        },
        "agents": {
            "deep": {
                "prompt": "Investigate this failure.",
                "permission": {
                    "rules": {
                        "task": "deny",
                        "shell": { "*": "deny", "cargo test *": "allow" }
                    }
                }
            },
            "looker": { "disable": true },
            "explore": {
                "mode": "subagent",
                "prompt": "A user-defined role with its own instructions.",
                "permission": { "rules": { "read": "ask" } }
            }
        }
    }));
    let actual = fixture.agents().await;
    assert!(!actual.iter().any(|agent| agent["id"] == "looker"));
    let deep = by_name(&actual, "deep");
    assert_eq!(deep["system"], "Investigate this failure.");
    let deep_rules = permission_rules(deep);
    for (permission, resource, effect) in [
        ("task", "*", PermissionAction::Deny),
        ("web_search", "*", PermissionAction::Deny),
        (
            "shell",
            "cargo test -p zuno-catalog",
            PermissionAction::Allow,
        ),
        ("shell", "cargo build", PermissionAction::Deny),
        ("goal_get", "*", PermissionAction::Allow),
    ] {
        assert_eq!(evaluate(permission, resource, &deep_rules), effect);
    }
    let custom = by_name(&actual, "explore");
    assert_eq!(
        custom["system"],
        "A user-defined role with its own instructions."
    );
    assert_eq!(custom["mode"], "subagent");
    let custom_rules = permission_rules(custom);
    assert_eq!(
        evaluate("read", "file.rs", &custom_rules),
        PermissionAction::Ask
    );
    assert_eq!(
        evaluate("shell", "*", &custom_rules),
        PermissionAction::Deny
    );
}

#[tokio::test]
async fn api_agent_catalog_keeps_markdown_and_environment_prompt_precedence() {
    let mut fixture = CatalogFixture::new();
    std::fs::write(
        fixture.directory.join("zuno.json"),
        r#"{"agents":{"title":{"prompt":"Project title prompt."}}}"#,
    )
    .expect("project config");
    let agents = fixture.directory.join(".zuno/agent");
    std::fs::create_dir_all(&agents).expect("Markdown agent directory");
    std::fs::write(agents.join("title.md"), "Markdown title prompt.\n")
        .expect("Markdown native override");
    let actual = fixture.agents().await;
    assert_eq!(
        by_name(&actual, "title")["system"],
        "Markdown title prompt."
    );
    fixture.configure(json!({
        "agents": { "title": { "prompt": "Environment title prompt.\n" } }
    }));
    let actual = fixture.agents().await;
    assert_eq!(
        by_name(&actual, "title")["system"],
        "Environment title prompt.\n"
    );
    assert_eq!(by_name(&actual, "title")["hidden"], true);
}

use codex_agent_extension::AgentBackendDescriptor;
use codex_agent_extension::OneShotAgentBackend;
use codex_core::config::Config;
use codex_plugin::EffectivePluginAgentBackend;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Map as JsonMap;
use serde_json::Value as JsonValue;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::sync::Arc;

const WORKFLOW_BINDING_SCHEMA: &str = "zuno.workflow-bindings/v1";
const WORKFLOW_BINDING_SCHEMA_VERSION: u64 = 1;
const ROUTE_BINDING_SCHEMA: &str = "zuno.workflow-agent-binding/v1";

/// Product-neutral inputs used to resolve one workflow route before run
/// admission. No prompt or invocation override participates in this binding.
#[derive(Debug, Clone)]
pub(crate) struct WorkflowAgentBindingRequest {
    pub(crate) parent_thread_id: String,
    pub(crate) route: String,
    pub(crate) agent_ref: String,
    pub(crate) execution_profile: Option<String>,
}

/// Frozen, non-secret identity of one resolved route backend and runtime policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WorkflowAgentBinding {
    pub(crate) digest: String,
    pub(crate) snapshot: JsonValue,
}

impl WorkflowAgentBinding {
    pub(crate) fn resolved(
        request: &WorkflowAgentBindingRequest,
        descriptor: &AgentBackendDescriptor,
        plugin: Option<&EffectivePluginAgentBackend>,
        environment_digest: Option<&str>,
        config: &Config,
    ) -> Result<Self, String> {
        let revision = descriptor.revision.as_deref().ok_or_else(|| {
            format!(
                "Agent backend factory `{}` has no stable revision",
                descriptor.id
            )
        })?;
        let snapshot = json!({
            "schema": ROUTE_BINDING_SCHEMA,
            "route": request.route,
            "agentRef": request.agent_ref,
            "executionProfile": request.execution_profile,
            "backend": {
                "id": descriptor.id,
                "kind": descriptor.kind,
                "revision": revision,
                "capabilities": descriptor.capabilities,
            },
            "plugin": plugin.map(plugin_binding),
            "environmentDigest": environment_digest,
            "model": config.model,
            "modelProvider": provider_binding(config)?,
            "reasoningEffort": config.model_reasoning_effort,
            "serviceTier": config.service_tier,
            "approvalPolicy": config.permissions.approval_policy.get(),
            "approvalsReviewer": config.approvals_reviewer,
            "activePermissionProfile": config.permissions.active_permission_profile(),
            "permissionProfile": config.permissions.effective_permission_profile(),
            "cwd": config.cwd.as_path().to_string_lossy(),
            "workspaceRoots": config.workspace_roots.iter()
                .map(|root| root.as_path().to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            "windowsSandbox": {
                "mode": config.permissions.windows_sandbox_mode,
                "type": config.permissions.windows_sandbox_type,
            },
            "processSandbox": {
                "codexSelfExecutable": config.codex_self_exe.as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
                "linuxSandboxExecutable": config.codex_linux_sandbox_exe.as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
                "managedNetworkConfigured": config.permissions.network.is_some(),
                "useLegacyLandlock": config.features.use_legacy_landlock(),
            },
        });
        Ok(Self {
            digest: canonical_digest(&snapshot)?,
            snapshot,
        })
    }
}

fn plugin_binding(plugin: &EffectivePluginAgentBackend) -> JsonValue {
    json!({
        "pluginId": plugin.plugin_identity.plugin_id,
        "remotePluginId": plugin.plugin_identity.remote_plugin_id,
        "version": plugin.declaration.plugin_version,
        "declarationPath": plugin.declaration.source_path,
        "declarationDigest": plugin.declaration.source_digest,
        "executableDigest": plugin.declaration.executable_digest,
        "sourceGeneration": plugin.declaration.source_generation,
    })
}

/// Complete route binding set persisted on an admitted workflow run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WorkflowAgentBindingSet {
    schema_version: u64,
    schema: String,
    routes: BTreeMap<String, WorkflowAgentBinding>,
}

impl WorkflowAgentBindingSet {
    pub(crate) fn new(routes: BTreeMap<String, WorkflowAgentBinding>) -> Self {
        Self {
            schema_version: WORKFLOW_BINDING_SCHEMA_VERSION,
            schema: WORKFLOW_BINDING_SCHEMA.to_string(),
            routes,
        }
    }

    pub(crate) fn to_json(&self) -> Result<JsonValue, String> {
        serde_json::to_value(self).map_err(|error| error.to_string())
    }

    #[cfg(test)]
    pub(crate) fn from_json(value: JsonValue) -> Result<Self, String> {
        let bindings: Self = serde_json::from_value(value).map_err(|error| error.to_string())?;
        if bindings.schema_version != WORKFLOW_BINDING_SCHEMA_VERSION
            || bindings.schema != WORKFLOW_BINDING_SCHEMA
        {
            return Err(format!(
                "unsupported workflow binding schema {:?} version {}",
                bindings.schema, bindings.schema_version
            ));
        }
        Ok(bindings)
    }
}

/// Runtime objects selected before durable admission for one route.
///
/// Constructing a backend does not start product work. Retaining this value
/// keeps the exact factory generation, effective profile, environment values,
/// and executable selection alive until the admitted run settles.
#[derive(Clone)]
pub(crate) struct PreparedWorkflowAgentBinding {
    binding: WorkflowAgentBinding,
    backend: Arc<dyn OneShotAgentBackend>,
    cwd: AbsolutePathBuf,
}

impl PreparedWorkflowAgentBinding {
    pub(crate) fn new(
        binding: WorkflowAgentBinding,
        backend: Arc<dyn OneShotAgentBackend>,
        cwd: AbsolutePathBuf,
    ) -> Self {
        Self {
            binding,
            backend,
            cwd,
        }
    }

    pub(crate) fn binding(&self) -> &WorkflowAgentBinding {
        &self.binding
    }

    pub(crate) fn backend(&self) -> &Arc<dyn OneShotAgentBackend> {
        &self.backend
    }

    pub(crate) fn cwd(&self) -> &AbsolutePathBuf {
        &self.cwd
    }
}

/// Persisted binding set paired with the exact in-process backend generations
/// that produced it. A process restart reconstructs this object only after the
/// persisted digest matches the newly resolved configuration.
pub(crate) struct PreparedWorkflowAgentBindingSet {
    persisted: WorkflowAgentBindingSet,
    routes: BTreeMap<String, PreparedWorkflowAgentBinding>,
}

impl PreparedWorkflowAgentBindingSet {
    pub(crate) fn new(routes: BTreeMap<String, PreparedWorkflowAgentBinding>) -> Self {
        let persisted = WorkflowAgentBindingSet::new(
            routes
                .iter()
                .map(|(name, prepared)| (name.clone(), prepared.binding().clone()))
                .collect(),
        );
        Self { persisted, routes }
    }

    pub(crate) fn persisted(&self) -> &WorkflowAgentBindingSet {
        &self.persisted
    }

    pub(crate) fn route(&self, name: &str) -> Option<&PreparedWorkflowAgentBinding> {
        self.routes.get(name)
    }
}

fn provider_binding(config: &Config) -> Result<JsonValue, String> {
    let mut provider = serde_json::to_value(&config.model_provider).map_err(|error| {
        format!(
            "model provider `{}` cannot be bound: {error}",
            config.model_provider_id
        )
    })?;
    let object = provider
        .as_object_mut()
        .ok_or_else(|| "model provider binding is not an object".to_string())?;

    // Bind the provider route and auth mechanism without persisting credentials,
    // header values, query values, or command arguments. Credential rotation is
    // deliberately not a workflow configuration change.
    redact_optional_value(object, "experimental_bearer_token");
    redact_map_values(object, "query_params");
    redact_map_values(object, "http_headers");
    redact_argument_values(object.get_mut("auth"));
    if let Some(aws) = object.get_mut("aws").and_then(JsonValue::as_object_mut) {
        redact_argument_values(aws.get_mut("auth_refresh"));
    }

    Ok(json!({
        "id": config.model_provider_id,
        "definition": provider,
    }))
}

fn redact_optional_value(object: &mut JsonMap<String, JsonValue>, field: &str) {
    if let Some(value) = object.get_mut(field) {
        *value = JsonValue::Bool(!value.is_null());
    }
}

fn redact_map_values(object: &mut JsonMap<String, JsonValue>, field: &str) {
    let Some(value) = object.get_mut(field) else {
        return;
    };
    let Some(values) = value.as_object() else {
        return;
    };
    *value = JsonValue::Object(
        values
            .keys()
            .map(|key| (key.clone(), JsonValue::Bool(true)))
            .collect(),
    );
}

fn redact_argument_values(value: Option<&mut JsonValue>) {
    let Some(object) = value.and_then(JsonValue::as_object_mut) else {
        return;
    };
    let Some(arguments) = object.get_mut("args") else {
        return;
    };
    let count = arguments.as_array().map_or(0, Vec::len);
    *arguments = json!({"redactedCount": count});
}

fn canonical_digest(value: &JsonValue) -> Result<String, String> {
    let canonical = canonical_json(value);
    serde_json::to_vec(&canonical)
        .map(|bytes| format!("{:x}", Sha256::digest(bytes)))
        .map_err(|error| error.to_string())
}

fn canonical_json(value: &JsonValue) -> JsonValue {
    match value {
        JsonValue::Array(values) => JsonValue::Array(values.iter().map(canonical_json).collect()),
        JsonValue::Object(values) => JsonValue::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), canonical_json(value)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        value => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_plugin::PluginAgentBackendDeclaration;
    use codex_plugin::PluginAgentBackendKind;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use codex_utils_plugins::PluginIdentity;

    #[test]
    fn binding_set_round_trip_is_key_order_independent() {
        let first = WorkflowAgentBinding {
            digest: canonical_digest(&json!({"b": 2, "a": 1})).expect("digest"),
            snapshot: json!({"b": 2, "a": 1}),
        };
        let second = WorkflowAgentBinding {
            digest: canonical_digest(&json!({"a": 1, "b": 2})).expect("digest"),
            snapshot: json!({"a": 1, "b": 2}),
        };
        assert_eq!(first.digest, second.digest);

        let bindings =
            WorkflowAgentBindingSet::new(BTreeMap::from([("review".to_string(), first)]));
        let restored = WorkflowAgentBindingSet::from_json(bindings.to_json().expect("serialize"))
            .expect("deserialize");
        assert_eq!(restored, bindings);
    }

    #[test]
    fn secret_bearing_provider_values_are_not_retained() {
        let mut provider = json!({
            "experimental_bearer_token": "secret",
            "query_params": {"token": "secret"},
            "http_headers": {"Authorization": "secret"},
            "auth": {"args": ["--token", "secret"]},
            "aws": {"auth_refresh": {"args": ["secret"]}}
        });
        let object = provider.as_object_mut().expect("object");
        redact_optional_value(object, "experimental_bearer_token");
        redact_map_values(object, "query_params");
        redact_map_values(object, "http_headers");
        redact_argument_values(object.get_mut("auth"));
        let aws = object["aws"].as_object_mut().expect("aws");
        redact_argument_values(aws.get_mut("auth_refresh"));

        let serialized = serde_json::to_string(&provider).expect("json");
        assert!(!serialized.contains("secret"));
        assert_eq!(provider["experimental_bearer_token"], true);
        assert_eq!(provider["auth"]["args"]["redactedCount"], 2);
    }

    #[test]
    fn plugin_binding_retains_exact_non_secret_attribution() {
        let declaration_path = AbsolutePathBuf::from_absolute_path_checked(
            std::env::temp_dir().join("zuno-plugin-agent-backends.json"),
        )
        .expect("absolute declaration path");
        let plugin = EffectivePluginAgentBackend {
            id: "team/review".to_string(),
            plugin_identity: PluginIdentity {
                plugin_id: "team@marketplace".to_string(),
                remote_plugin_id: Some("plugins~Plugin_team".to_string()),
            },
            declaration: PluginAgentBackendDeclaration {
                local_id: "review".to_string(),
                kind: PluginAgentBackendKind::Acp,
                plugin_version: Some("1.2.3".to_string()),
                source_path: declaration_path.clone(),
                source_digest: "declaration-sha256".to_string(),
                executable_digest: Some("executable-sha256".to_string()),
                source_generation: "source-generation".to_string(),
                command: None,
                command_windows: None,
                args: Vec::new(),
                env_vars: vec!["SECRET_TOKEN".to_string()],
                startup_timeout_ms: 20_000,
                run_timeout_ms: None,
                dispose_grace_ms: 3_000,
                max_message_bytes: 8 * 1024 * 1024,
            },
        };

        let binding = plugin_binding(&plugin);

        assert_eq!(binding["pluginId"], "team@marketplace");
        assert_eq!(binding["remotePluginId"], "plugins~Plugin_team");
        assert_eq!(binding["version"], "1.2.3");
        assert_eq!(binding["declarationPath"], json!(declaration_path));
        assert_eq!(binding["declarationDigest"], "declaration-sha256");
        assert_eq!(binding["executableDigest"], "executable-sha256");
        assert_eq!(binding["sourceGeneration"], "source-generation");
        assert!(!binding.to_string().contains("SECRET_TOKEN"));
    }
}

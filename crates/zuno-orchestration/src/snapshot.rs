//! Immutable, serializable orchestration identity shared across one execution tree.
//!
//! This module contains data only. It deliberately cannot schedule work, resolve a
//! provider, authorize a tool, read credentials, or hold a database/runtime handle.
//! The composition root freezes capability descriptors here; the engine adds the
//! final prompt and tool identities at the provider-request boundary.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Version of the persisted orchestration snapshot contract.
pub const SNAPSHOT_SCHEMA_VERSION: u32 = 4;

/// Stable digest identifying one immutable snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SnapshotIdentity {
    /// Snapshot schema used before hashing.
    pub schema_version: u32,
    /// Lowercase SHA-256 of canonical JSON.
    pub sha256: String,
}

/// Identity of the descriptor pack that contributed first-party metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PackIdentity {
    pub id: String,
    pub version: String,
    pub upstream_revision: String,
}

/// One Agent Profile contribution in a capability generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProfileDescriptor {
    pub name: String,
    pub source_id: String,
    pub definition_sha256: String,
    pub permission_sha256: String,
    pub tools: Option<Vec<String>>,
    pub delegates: Option<Vec<String>>,
}

/// One immutable node in a workflow template.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowNodeDescriptor {
    pub id: String,
    pub agent: String,
    pub prompt: Option<String>,
    pub description: Option<String>,
    pub depends_on: Vec<String>,
}

/// One configuration- or pack-owned workflow descriptor.
///
/// Runtime scheduling authority remains outside this type. It records the graph
/// and bounds a scheduler is allowed to instantiate, but cannot execute either.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowTemplateDescriptor {
    pub name: String,
    pub source_id: String,
    pub max_parallel: usize,
    pub max_agents: usize,
    pub nodes: Vec<WorkflowNodeDescriptor>,
}

/// One immutable expert seat in a Council preset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CouncilSeatDescriptor {
    pub id: String,
    pub agent: String,
    pub instruction: String,
}

/// Runtime-owned retry bounds for one Council seat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CouncilRetryPolicyDescriptor {
    pub max_retries: usize,
}

/// Bounds applied before structured seat results reach the synthesizer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CouncilSynthesisPolicyDescriptor {
    /// Maximum wall-clock time reserved for synthesis inside the Council deadline.
    pub timeout_ms: u64,
    pub max_input_bytes: usize,
}

/// One configuration- or pack-owned Council preset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CouncilPresetDescriptor {
    pub name: String,
    pub source_id: String,
    pub quorum: usize,
    pub max_parallel: usize,
    pub deadline_ms: u64,
    pub seat_output_bytes: usize,
    pub retry_policy: CouncilRetryPolicyDescriptor,
    pub synthesis_policy: CouncilSynthesisPolicyDescriptor,
    pub seats: Vec<CouncilSeatDescriptor>,
}

/// One Skill advertised by a frozen capability generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillCapabilityDescriptor {
    pub name: String,
    pub source: String,
    pub metadata_sha256: String,
    /// Known for embedded resources; selected file-backed Skills are hashed in the
    /// Attempt after their body has actually been loaded.
    pub content_sha256: Option<String>,
}

/// One model route inside a named preset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PresetRouteDescriptor {
    pub target: String,
    pub model: String,
    pub reasoning: Option<String>,
}

/// Complete immutable contents of one preset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PresetDescriptor {
    pub name: String,
    pub agents: Vec<PresetRouteDescriptor>,
    pub categories: Vec<PresetRouteDescriptor>,
}

impl PresetDescriptor {
    /// Digest used by the selected Attempt without duplicating every route.
    pub fn identity(&self) -> Result<SnapshotIdentity, serde_json::Error> {
        snapshot_identity(SNAPSHOT_SCHEMA_VERSION, self)
    }
}

/// The selected preset and the exact route table it referred to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PresetSelection {
    pub name: String,
    pub sha256: String,
}

/// Maximum sandbox authority frozen with one capability generation.
///
/// Paths remain in their validated configuration spelling here. The shell runtime
/// resolves them against the active workspace, while this descriptor only ensures a
/// delegated child cannot silently observe a broader configuration generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SandboxCapabilityDescriptor {
    pub mode: String,
    pub network: String,
    pub writable_roots: Vec<String>,
    pub protected_paths: Vec<String>,
}

impl Default for SandboxCapabilityDescriptor {
    fn default() -> Self {
        Self {
            mode: "workspace-write".to_owned(),
            network: "deny".to_owned(),
            writable_roots: Vec::new(),
            protected_paths: Vec::new(),
        }
    }
}

/// The immutable capability catalogue shared by a parent and every admitted child.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilityContents {
    pub sandbox: SandboxCapabilityDescriptor,
    pub profiles: Vec<ProfileDescriptor>,
    pub presets: Vec<PresetDescriptor>,
    pub councils: Vec<CouncilPresetDescriptor>,
    pub workflows: Vec<WorkflowTemplateDescriptor>,
    pub skills: Vec<SkillCapabilityDescriptor>,
}

/// The immutable capability catalogue shared by a parent and every admitted child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapabilitySnapshot {
    pub schema_version: u32,
    pub pack: PackIdentity,
    pub extension_revision: u64,
    pub permission_policy_sha256: String,
    pub sandbox: SandboxCapabilityDescriptor,
    pub profiles: Vec<ProfileDescriptor>,
    pub presets: Vec<PresetDescriptor>,
    pub councils: Vec<CouncilPresetDescriptor>,
    pub workflows: Vec<WorkflowTemplateDescriptor>,
    pub skills: Vec<SkillCapabilityDescriptor>,
}

impl CapabilitySnapshot {
    /// Construct a deterministic set identity while preserving workflow-node order.
    #[must_use]
    pub fn new(
        pack: PackIdentity,
        extension_revision: u64,
        permission_policy_sha256: impl Into<String>,
        mut contents: CapabilityContents,
    ) -> Self {
        contents.profiles.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.source_id.cmp(&right.source_id))
        });
        contents.workflows.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.source_id.cmp(&right.source_id))
        });
        contents.councils.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.source_id.cmp(&right.source_id))
        });
        contents
            .presets
            .sort_by(|left, right| left.name.cmp(&right.name));
        contents.skills.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.source.cmp(&right.source))
        });
        contents.sandbox.writable_roots.sort();
        contents.sandbox.writable_roots.dedup();
        contents.sandbox.protected_paths.sort();
        contents.sandbox.protected_paths.dedup();
        Self {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            pack,
            extension_revision,
            permission_policy_sha256: permission_policy_sha256.into(),
            sandbox: contents.sandbox,
            profiles: contents.profiles,
            presets: contents.presets,
            councils: contents.councils,
            workflows: contents.workflows,
            skills: contents.skills,
        }
    }

    /// Canonical identity used for parent/child drift checks.
    pub fn identity(&self) -> Result<SnapshotIdentity, serde_json::Error> {
        snapshot_identity(self.schema_version, self)
    }
}

/// The exact Agent Profile selected for one provider Attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentAttemptIdentity {
    pub name: String,
    pub source_id: String,
    pub definition_sha256: String,
    pub permission_sha256: String,
    pub prompt_policy_sha256: String,
}

/// The exact provider/model/reasoning route selected for one Attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelAttemptIdentity {
    pub provider_id: String,
    pub model_id: String,
    pub wire_model_id: String,
    pub surface: String,
    pub reasoning_sha256: String,
    pub preset: Option<PresetSelection>,
}

/// One fully loaded Skill whose body entered the prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SelectedSkillIdentity {
    pub name: String,
    pub source: String,
    pub content_sha256: String,
}

/// One exact provider-visible tool definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolSchemaIdentity {
    pub name: String,
    pub description_sha256: String,
    pub schema_sha256: String,
    pub ui_intent: String,
}

/// Durable ownership coordinates for an Attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OwnerLineage {
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub parent_attempt: Option<SnapshotIdentity>,
    pub workflow: Option<String>,
    pub workflow_node: Option<String>,
}

/// Prompt receipt and exact post-hook prompt identities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PromptReceiptIdentity {
    pub event_id: Option<String>,
    pub assembly_sha256: String,
    pub actual_sha256: String,
}

/// Composition-root facts known before the provider-visible prompt and tool set exist.
///
/// The engine consumes this seed exactly once to finalize an [`AttemptSnapshot`]. A
/// child receives the parent's completed snapshot and may only proceed when its newly
/// resolved capability generation still has the same identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttemptSeed {
    pub capability: CapabilitySnapshot,
    pub agent: AgentAttemptIdentity,
    pub preset: Option<PresetSelection>,
    #[serde(default = "default_subagent_model_policy_sha256")]
    pub subagent_model_policy_sha256: String,
    pub parent_attempt: Option<SnapshotIdentity>,
    pub workflow: Option<String>,
    pub workflow_node: Option<String>,
}

/// Complete immutable identity for one provider request Attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttemptSnapshot {
    pub schema_version: u32,
    pub turn_id: String,
    pub step: u32,
    pub capability: CapabilitySnapshot,
    pub owner: OwnerLineage,
    pub agent: AgentAttemptIdentity,
    pub model: ModelAttemptIdentity,
    #[serde(default = "default_subagent_model_policy_sha256")]
    pub subagent_model_policy_sha256: String,
    pub selected_skills: Vec<SelectedSkillIdentity>,
    pub prompt: PromptReceiptIdentity,
    /// Preserves provider-visible order rather than sorting by name.
    pub tools: Vec<ToolSchemaIdentity>,
}

fn default_subagent_model_policy_sha256() -> String {
    sha256_json(&serde_json::json!({
        "enabled": false,
        "allowedModels": [],
    }))
}

impl AttemptSnapshot {
    /// Canonical identity persisted with provider requests and background jobs.
    pub fn identity(&self) -> Result<SnapshotIdentity, serde_json::Error> {
        snapshot_identity(self.schema_version, self)
    }

    /// Canonical JSON value suitable for durable event storage.
    pub fn canonical_value(&self) -> Result<Value, serde_json::Error> {
        serde_json::to_value(self).map(canonicalize)
    }
}

/// Lowercase SHA-256 of UTF-8 text.
#[must_use]
pub fn sha256_text(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// Lowercase SHA-256 of canonical JSON, with every object key sorted recursively.
#[must_use]
pub fn sha256_json(value: &Value) -> String {
    let canonical = canonicalize(value.clone());
    let bytes = serde_json::to_vec(&canonical)
        .expect("serializing an owned JSON value to bytes cannot fail");
    hex::encode(Sha256::digest(bytes))
}

fn snapshot_identity<T: Serialize>(
    schema_version: u32,
    value: &T,
) -> Result<SnapshotIdentity, serde_json::Error> {
    let value = canonicalize(serde_json::to_value(value)?);
    Ok(SnapshotIdentity {
        schema_version,
        sha256: sha256_json(&value),
    })
}

fn canonicalize(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize).collect()),
        Value::Object(values) => {
            let mut entries = values.into_iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, canonicalize(value)))
                    .collect::<Map<_, _>>(),
            )
        }
        scalar => scalar,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack() -> PackIdentity {
        PackIdentity {
            id: "zuno-orchestration".to_owned(),
            version: "0.1.0".to_owned(),
            upstream_revision: "zuno@test".to_owned(),
        }
    }

    fn profile(name: &str) -> ProfileDescriptor {
        ProfileDescriptor {
            name: name.to_owned(),
            source_id: format!("builtin://agent/{name}"),
            definition_sha256: sha256_text(&format!("definition:{name}")),
            permission_sha256: sha256_text(&format!("permission:{name}")),
            tools: Some(vec!["read".to_owned()]),
            delegates: None,
        }
    }

    fn skill(name: &str) -> SkillCapabilityDescriptor {
        SkillCapabilityDescriptor {
            name: name.to_owned(),
            source: format!("builtin://skill/{name}"),
            metadata_sha256: sha256_text(&format!("metadata:{name}")),
            content_sha256: None,
        }
    }

    fn council(name: &str) -> CouncilPresetDescriptor {
        CouncilPresetDescriptor {
            name: name.to_owned(),
            source_id: format!("builtin://council/{name}"),
            quorum: 2,
            max_parallel: 3,
            deadline_ms: 120_000,
            seat_output_bytes: 16_384,
            retry_policy: CouncilRetryPolicyDescriptor { max_retries: 1 },
            synthesis_policy: CouncilSynthesisPolicyDescriptor {
                timeout_ms: 60_000,
                max_input_bytes: 32_768,
            },
            seats: vec![CouncilSeatDescriptor {
                id: "evidence".to_owned(),
                agent: "explorer".to_owned(),
                instruction: "Collect implementation evidence.".to_owned(),
            }],
        }
    }

    fn capability() -> CapabilitySnapshot {
        CapabilitySnapshot::new(
            pack(),
            7,
            sha256_text("permission policy"),
            CapabilityContents {
                profiles: vec![profile("orchestrator")],
                councils: vec![council("balanced-review")],
                workflows: vec![WorkflowTemplateDescriptor {
                    name: "release".to_owned(),
                    source_id: "config://workflows/release".to_owned(),
                    max_parallel: 2,
                    max_agents: 4,
                    nodes: vec![WorkflowNodeDescriptor {
                        id: "scan".to_owned(),
                        agent: "explorer".to_owned(),
                        prompt: None,
                        description: None,
                        depends_on: Vec::new(),
                    }],
                }],
                skills: vec![skill("codemap")],
                ..CapabilityContents::default()
            },
        )
    }

    fn attempt() -> AttemptSnapshot {
        AttemptSnapshot {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            turn_id: "turn-1".to_owned(),
            step: 1,
            capability: capability(),
            owner: OwnerLineage {
                session_id: "session-1".to_owned(),
                parent_session_id: None,
                parent_attempt: None,
                workflow: None,
                workflow_node: None,
            },
            agent: AgentAttemptIdentity {
                name: "orchestrator".to_owned(),
                source_id: "builtin://agent/orchestrator".to_owned(),
                definition_sha256: sha256_text("agent"),
                permission_sha256: sha256_text("rules"),
                prompt_policy_sha256: sha256_text("policy"),
            },
            model: ModelAttemptIdentity {
                provider_id: "myopenai".to_owned(),
                model_id: "gpt-5.6-sol".to_owned(),
                wire_model_id: "gpt-5.6-sol".to_owned(),
                surface: "responses".to_owned(),
                reasoning_sha256: sha256_text("max"),
                preset: Some(PresetSelection {
                    name: "house".to_owned(),
                    sha256: sha256_text("house routes"),
                }),
            },
            subagent_model_policy_sha256: sha256_text("subagent-model-policy"),
            selected_skills: vec![SelectedSkillIdentity {
                name: "codemap".to_owned(),
                source: "builtin://skill/codemap".to_owned(),
                content_sha256: sha256_text("skill body"),
            }],
            prompt: PromptReceiptIdentity {
                event_id: Some("evt-1".to_owned()),
                assembly_sha256: sha256_text("assembly"),
                actual_sha256: sha256_text("actual"),
            },
            tools: vec![ToolSchemaIdentity {
                name: "read".to_owned(),
                description_sha256: sha256_text("read files"),
                schema_sha256: sha256_json(&serde_json::json!({"type":"object"})),
                ui_intent: "generic".to_owned(),
            }],
        }
    }

    #[test]
    fn capability_set_order_is_canonical_but_workflow_node_order_is_not_erased() {
        let left = CapabilitySnapshot::new(
            pack(),
            7,
            sha256_text("permission policy"),
            CapabilityContents {
                profiles: vec![profile("plan"), profile("build")],
                councils: vec![council("zeta"), council("alpha")],
                skills: vec![skill("reflect"), skill("codemap")],
                ..CapabilityContents::default()
            },
        );
        let right = CapabilitySnapshot::new(
            pack(),
            7,
            sha256_text("permission policy"),
            CapabilityContents {
                profiles: vec![profile("build"), profile("plan")],
                councils: vec![council("alpha"), council("zeta")],
                skills: vec![skill("codemap"), skill("reflect")],
                ..CapabilityContents::default()
            },
        );
        assert_eq!(left, right);
        assert_eq!(
            left.identity().expect("left identity"),
            right.identity().expect("right identity")
        );
    }

    #[test]
    fn attempt_identity_changes_for_model_reasoning_skill_and_tool_schema() {
        let original = attempt();
        let identity = original.identity().expect("identity");

        let mut model = original.clone();
        model.model.model_id = "gpt-5.6-terra".to_owned();
        assert_ne!(identity, model.identity().expect("model identity"));

        let mut reasoning = original.clone();
        reasoning.model.reasoning_sha256 = sha256_text("high");
        assert_ne!(identity, reasoning.identity().expect("reasoning identity"));

        let mut selected_skill = original.clone();
        selected_skill.selected_skills[0].content_sha256 = sha256_text("changed body");
        assert_ne!(identity, selected_skill.identity().expect("skill identity"));

        let mut tool = original;
        tool.tools[0].schema_sha256 = sha256_json(&serde_json::json!({
            "type":"object",
            "required":["path"]
        }));
        assert_ne!(identity, tool.identity().expect("tool identity"));
    }

    #[test]
    fn capability_identity_changes_when_the_sandbox_authority_changes() {
        let original = capability();
        let identity = original.identity().expect("identity");

        let mut mode = original.clone();
        mode.sandbox.mode = "danger-full-access".to_owned();
        assert_ne!(identity, mode.identity().expect("sandbox mode identity"));

        let mut network = original.clone();
        network.sandbox.network = "allow".to_owned();
        assert_ne!(
            identity,
            network.identity().expect("sandbox network identity")
        );

        let mut writable = original;
        writable
            .sandbox
            .writable_roots
            .push("../shared-cache".to_owned());
        assert_ne!(
            identity,
            writable.identity().expect("sandbox writable-root identity")
        );
    }

    #[test]
    fn snapshot_json_contains_identity_only_and_round_trips() {
        let snapshot = attempt();
        let value = snapshot.canonical_value().expect("canonical snapshot");
        let text = serde_json::to_string(&value).expect("json");
        assert!(!text.contains("apiKey"));
        assert!(!text.contains("accessToken"));
        let decoded: AttemptSnapshot = serde_json::from_value(value).expect("round trip");
        assert_eq!(snapshot, decoded);
    }
}

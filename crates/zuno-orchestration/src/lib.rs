//! Static first-party orchestration metadata and concise Skill resources.
//!
//! This crate deliberately contains no scheduler, runtime service, permission
//! mutation, provider client, or plugin lifecycle. Consumers may advertise these
//! descriptors only after independently checking the active Agent profile and its
//! enforced tool visibility.

mod snapshot;

pub use snapshot::{
    AgentAttemptIdentity, AttemptSeed, AttemptSnapshot, CapabilitySnapshot, ModelAttemptIdentity,
    OwnerLineage, PackIdentity, PresetDescriptor, PresetRouteDescriptor, PresetSelection,
    ProfileDescriptor, PromptReceiptIdentity, SNAPSHOT_SCHEMA_VERSION, SelectedSkillIdentity,
    SkillCapabilityDescriptor, SnapshotIdentity, ToolSchemaIdentity, WorkflowNodeDescriptor,
    WorkflowTemplateDescriptor, sha256_json, sha256_text,
};

/// Stable identifier for the first-party pack.
pub const PACK_ID: &str = "zuno-orchestration";

/// Version of the descriptors and embedded Skill resources.
pub const PACK_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Zuno source revision used when the original Skill text was authored.
pub const UPSTREAM_REVISION: &str = "zuno@ef709e571d40c2cd9fbf12ad5e4c6de81cd498d9";

/// License review shared by the original first-party resources.
pub const LICENSE_REVIEW: &str =
    "Original Zuno text under the repository MIT license; no upstream Skill or prompt body copied.";

/// Provenance attached to one first-party Skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkillProvenance {
    /// Design source that informed the original Zuno-specific instructions.
    pub inspiration: &'static str,
    /// Human-reviewed licensing statement for the embedded body.
    pub license_review: &'static str,
    /// Source revision against which implemented Zuno capabilities were checked.
    pub upstream_revision: &'static str,
}

/// One immutable first-party Skill contribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinSkillDescriptor {
    /// Model-facing selection name.
    pub name: &'static str,
    /// Short trigger and purpose shown in a Skill catalog.
    pub description: &'static str,
    /// Original, concise Markdown instruction body.
    pub content: &'static str,
    /// Stable logical identity, including pack and version.
    pub source_id: &'static str,
    /// Unique catalog location used to distinguish same-named external Skills.
    pub location: &'static str,
    /// Agent profiles for which the Skill may be advertised.
    pub allowed_profiles: &'static [&'static str],
    /// Tools that must already be visible before the Skill may be selected.
    pub required_tools: &'static [&'static str],
    /// Lowercase hexadecimal SHA-256 digest of [`Self::content`].
    pub content_sha256: &'static str,
    /// Authorship, license review, and source revision.
    pub provenance: SkillProvenance,
}

/// A static first-party pack. It contributes data only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirstPartyOrchestrationPack {
    /// Stable pack identity.
    pub id: &'static str,
    /// Pack resource version.
    pub version: &'static str,
    /// Deterministically ordered Skill descriptors.
    pub skills: &'static [BuiltinSkillDescriptor],
}

const USER_FACING_PROFILES: &[&str] = &[
    "orchestrator",
    "build",
    "plan",
    "deep",
    "fixer",
    "general",
    "explorer",
    "librarian",
    "oracle",
    "looker",
];

const DEEPWORK_PROFILES: &[&str] = &["orchestrator", "build", "plan", "deep", "general"];

const CODEMAP_PROFILES: &[&str] = &[
    "orchestrator",
    "build",
    "plan",
    "deep",
    "fixer",
    "general",
    "explorer",
    "librarian",
    "oracle",
];

const VERIFICATION_PROFILES: &[&str] = &[
    "orchestrator",
    "build",
    "plan",
    "deep",
    "fixer",
    "general",
    "oracle",
];

const REFLECT_PROFILES: &[&str] = &["orchestrator", "build", "deep", "general", "oracle"];

const MUTATING_WORK_PROFILES: &[&str] = &["orchestrator", "build", "deep", "fixer", "general"];

const NATIVE_PROVENANCE: SkillProvenance = SkillProvenance {
    inspiration: "Zuno's native Rust capability, prompt, work-state, memory, and lifecycle contracts.",
    license_review: LICENSE_REVIEW,
    upstream_revision: UPSTREAM_REVISION,
};

/// Every Skill shipped by the first-party pack, in stable presentation order.
pub const SKILLS: [BuiltinSkillDescriptor; 7] = [
    BuiltinSkillDescriptor {
        name: "customize-zuno",
        description: "Inspect or change Zuno configuration, providers, authentication, permissions, Agents, workflows, Skills, MCP servers, or extensions.",
        content: include_str!("skills/customize-zuno.md"),
        source_id: "zuno-orchestration:skill/customize-zuno@0.1.0",
        location: "builtin://zuno-orchestration/0.1.0/customize-zuno",
        allowed_profiles: USER_FACING_PROFILES,
        required_tools: &["read", "glob", "grep"],
        content_sha256: "b20e2eb8c99ea75982de36ad0b49181099dff467420de83aef0cbd86342b554d",
        provenance: NATIVE_PROVENANCE,
    },
    BuiltinSkillDescriptor {
        name: "deepwork",
        description: "Turn a bounded complex request into durable Goal, Plan, Todo, ownership, dependency, and verification state.",
        content: include_str!("skills/deepwork.md"),
        source_id: "zuno-orchestration:skill/deepwork@0.1.0",
        location: "builtin://zuno-orchestration/0.1.0/deepwork",
        allowed_profiles: DEEPWORK_PROFILES,
        required_tools: &["plan_get", "plan_update", "todo_get", "todo_update"],
        content_sha256: "57320629e2f11d7556a2b98217508093f1123828d49165beedda856f48f4a7ea",
        provenance: NATIVE_PROVENANCE,
    },
    BuiltinSkillDescriptor {
        name: "codemap",
        description: "Use the native CodeGraph index and read-only tools to return a scoped structural code map with evidence.",
        content: include_str!("skills/codemap.md"),
        source_id: "zuno-orchestration:skill/codemap@0.1.0",
        location: "builtin://zuno-orchestration/0.1.0/codemap",
        allowed_profiles: CODEMAP_PROFILES,
        required_tools: &["read", "glob", "grep"],
        content_sha256: "f6391d18439d06cf3c90ed1faa21b3a909e7eec8be5d928c5cf576d548c7ae9c",
        provenance: NATIVE_PROVENANCE,
    },
    BuiltinSkillDescriptor {
        name: "verification-planning",
        description: "Define risk-proportional evidence, commands, fixtures, expected outputs, and acceptance surfaces before delivery.",
        content: include_str!("skills/verification-planning.md"),
        source_id: "zuno-orchestration:skill/verification-planning@0.1.0",
        location: "builtin://zuno-orchestration/0.1.0/verification-planning",
        allowed_profiles: VERIFICATION_PROFILES,
        required_tools: &["read"],
        content_sha256: "1f2a2b26e66c965752a016e896684f4cec158a09caf0438c75000c2f5ff1d27f",
        provenance: NATIVE_PROVENANCE,
    },
    BuiltinSkillDescriptor {
        name: "reflect",
        description: "Extract bounded, reviewable memory candidates from confirmed outcomes without silently changing code or prompts.",
        content: include_str!("skills/reflect.md"),
        source_id: "zuno-orchestration:skill/reflect@0.1.0",
        location: "builtin://zuno-orchestration/0.1.0/reflect",
        allowed_profiles: REFLECT_PROFILES,
        required_tools: &["read"],
        content_sha256: "5a4c1ba0f89a8c3b5f83579a48ef32504a34cc3b926b50fecdeb1faab0d3a3f8",
        provenance: NATIVE_PROVENANCE,
    },
    BuiltinSkillDescriptor {
        name: "worktree",
        description: "Perform read-only worktree preflight checks; the current pack does not create, lease, or clean worktrees.",
        content: include_str!("skills/worktree.md"),
        source_id: "zuno-orchestration:skill/worktree@0.1.0",
        location: "builtin://zuno-orchestration/0.1.0/worktree",
        allowed_profiles: MUTATING_WORK_PROFILES,
        required_tools: &["read", "bash"],
        content_sha256: "fdba7050f32ecb96db4f13fad719896db3b8c5e71d0faaed23022889066d000f",
        provenance: NATIVE_PROVENANCE,
    },
    BuiltinSkillDescriptor {
        name: "git-workflow",
        description: "Inspect repository state, preserve user changes, scope commits, and verify staged delivery without destructive cleanup.",
        content: include_str!("skills/git-workflow.md"),
        source_id: "zuno-orchestration:skill/git-workflow@0.1.0",
        location: "builtin://zuno-orchestration/0.1.0/git-workflow",
        allowed_profiles: MUTATING_WORK_PROFILES,
        required_tools: &["read", "bash"],
        content_sha256: "0561c6522da0501cb5a0f68a7c96fa1181a9330f1e2c429952f255ebbd4e6cbf",
        provenance: NATIVE_PROVENANCE,
    },
];

/// The complete static first-party pack.
pub const PACK: FirstPartyOrchestrationPack = FirstPartyOrchestrationPack {
    id: PACK_ID,
    version: PACK_VERSION,
    skills: &SKILLS,
};

/// Borrow the complete static first-party pack.
#[must_use]
pub const fn pack() -> &'static FirstPartyOrchestrationPack {
    &PACK
}

/// Borrow the stable ordered Skill catalog.
#[must_use]
pub const fn skills() -> &'static [BuiltinSkillDescriptor] {
    &SKILLS
}

/// Find one built-in Skill by its exact name.
#[must_use]
pub fn skill(name: &str) -> Option<&'static BuiltinSkillDescriptor> {
    SKILLS.iter().find(|skill| skill.name == name)
}

//! Static first-party orchestration metadata and concise Skill resources.
//!
//! This crate deliberately contains no scheduler, runtime service, permission
//! mutation, provider client, or plugin lifecycle. Consumers may advertise these
//! descriptors only after independently checking the active Agent profile and its
//! enforced tool visibility.

mod snapshot;

pub use snapshot::{
    AgentAttemptIdentity, AttemptSeed, AttemptSnapshot, CapabilityContents, CapabilitySnapshot,
    CouncilPresetDescriptor, CouncilRetryPolicyDescriptor, CouncilSeatDescriptor,
    CouncilSynthesisPolicyDescriptor, ModelAttemptIdentity, OwnerLineage, PackIdentity,
    PresetDescriptor, PresetRouteDescriptor, PresetSelection, ProfileDescriptor,
    PromptReceiptIdentity, SNAPSHOT_SCHEMA_VERSION, SandboxCapabilityDescriptor,
    SelectedSkillIdentity, SkillCapabilityDescriptor, SnapshotIdentity, ToolSchemaIdentity,
    WorkflowNodeDescriptor, WorkflowTemplateDescriptor, sha256_json, sha256_text,
};

/// Stable identifier for the first-party pack.
pub const PACK_ID: &str = "zuno-orchestration";

/// Version of the descriptors and embedded Skill resources.
pub const PACK_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Zuno source revision against which the embedded capability requirements were reviewed.
pub const CAPABILITY_REVIEW_REVISION: &str = "zuno@eb177e833035ea36aa8c37156d2c131acaaaebac";

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
    /// Tools that must already be registered and permission-visible before selection.
    pub required_tools: &'static [&'static str],
    /// Lowercase hexadecimal SHA-256 digest of [`Self::content`].
    pub content_sha256: &'static str,
    /// Authorship, license review, and source revision.
    pub provenance: SkillProvenance,
}

/// One immutable expert seat in a first-party Council preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinCouncilSeatDescriptor {
    /// Stable preset-local identity used for ordering and durable work items.
    pub id: &'static str,
    /// Canonical delegable Agent profile.
    pub agent: &'static str,
    /// Original Zuno instruction appended to the shared question.
    pub instruction: &'static str,
}

/// One bounded first-party Council preset.
///
/// The caller may select `name` and provide the question. Seats, quorum,
/// concurrency, retry, the end-to-end deadline, and synthesis bounds remain
/// pack-owned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinCouncilPresetDescriptor {
    pub name: &'static str,
    pub description: &'static str,
    pub source_id: &'static str,
    pub seats: &'static [BuiltinCouncilSeatDescriptor],
    pub quorum: usize,
    pub max_parallel: usize,
    pub deadline_ms: u64,
    pub synthesis_timeout_ms: u64,
    pub max_retries: usize,
    pub seat_output_bytes: usize,
    pub synthesis_input_bytes: usize,
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
    /// Deterministically ordered, configuration-owned Council presets.
    pub councils: &'static [BuiltinCouncilPresetDescriptor],
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

/// Profiles whose shipped permission overlay lets them read durable Goal state.
///
/// `deepwork` requires `goal_get` (its body opens with "Read the current Goal, Plan,
/// and Todo state"). `orchestrator` and `build` see it through the allow-all base;
/// `plan` and `deep` name it in their deny-by-default overlays. `general` grants the
/// other four required tools but not this one, so declaring it there would advertise a
/// Skill that the visibility gate drops on every turn (audit OH-01). The contract test
/// in `zuno-cli` (`agent::tests`) runs that gate against the real overlays for every
/// declared pair.
const DEEPWORK_PROFILES: &[&str] = &["orchestrator", "build", "plan", "deep"];

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

/// Profiles whose shipped permission overlay lets them call `capability_claim`.
///
/// No deny-by-default overlay names `capability_claim`, so only the two profiles
/// that inherit the allow-all base can record a claim. `deep`, `fixer`, and `general`
/// hold `edit` and could make the write this Skill governs, but they cannot record
/// the evidence it demands, and advertising the Skill to them was a promise the
/// visibility gate silently withdrew (audit OH-02). Kept separate from
/// [`MUTATING_WORK_PROFILES`], whose other two Skills need only `read` and `shell`.
const CAPABILITY_CLAIM_PROFILES: &[&str] = &["orchestrator", "build"];

const UI_DESIGN_PROFILES: &[&str] = &[
    "orchestrator",
    "build",
    "plan",
    "deep",
    "fixer",
    "general",
    "oracle",
    "looker",
];

const NATIVE_PROVENANCE: SkillProvenance = SkillProvenance {
    inspiration: "Zuno's native Rust capability, prompt, work-state, memory, and lifecycle contracts.",
    license_review: LICENSE_REVIEW,
    upstream_revision: CAPABILITY_REVIEW_REVISION,
};

const CODEX_GITHUB_PROVENANCE: SkillProvenance = SkillProvenance {
    inspiration: "OpenAI Codex GitHub Action and non-interactive CI guidance, adapted to Zuno's native durable runtime.",
    license_review: LICENSE_REVIEW,
    upstream_revision: "OpenAI Codex documentation reviewed 2026-08-31; zuno@dccd8c49317c2035fc57512c27cb7cb3c4e249f7",
};

macro_rules! pack_source_id {
    ($resource:literal) => {
        concat!(
            "zuno-orchestration:",
            $resource,
            "@",
            env!("CARGO_PKG_VERSION")
        )
    };
}

macro_rules! pack_location {
    ($name:literal) => {
        concat!(
            "builtin://zuno-orchestration/",
            env!("CARGO_PKG_VERSION"),
            "/",
            $name
        )
    };
}

/// Every Skill shipped by the first-party pack, in stable presentation order.
pub const SKILLS: [BuiltinSkillDescriptor; 11] = [
    BuiltinSkillDescriptor {
        name: "customize-zuno",
        description: "Inspect or change Zuno configuration, providers, authentication, permissions, Agents, workflows, Skills, MCP servers, or extensions.",
        content: include_str!("skills/customize-zuno.md"),
        source_id: pack_source_id!("skill/customize-zuno"),
        location: pack_location!("customize-zuno"),
        allowed_profiles: USER_FACING_PROFILES,
        required_tools: &["read", "glob", "grep"],
        content_sha256: "2d95bcf802d8d49e9c5a0c46a6105d234977a45a1fa6ef4eb025f33b83d46360",
        provenance: NATIVE_PROVENANCE,
    },
    BuiltinSkillDescriptor {
        name: "develop-zuno",
        description: "Design or implement native Zuno configuration, Agents, Skills, providers, MCP integrations, extension plugins, or runtime extension points.",
        content: include_str!("skills/develop-zuno.md"),
        source_id: pack_source_id!("skill/develop-zuno"),
        location: pack_location!("develop-zuno"),
        allowed_profiles: USER_FACING_PROFILES,
        required_tools: &["read", "glob", "grep"],
        content_sha256: "bd92eb5b2988d57385d666e7e0ae1a26aad7349f1bfee23294e481998b83a13b",
        provenance: NATIVE_PROVENANCE,
    },
    BuiltinSkillDescriptor {
        name: "deepwork",
        description: "Turn a bounded complex request into durable Goal, Plan, Todo, ownership, dependency, and verification state.",
        content: include_str!("skills/deepwork.md"),
        source_id: pack_source_id!("skill/deepwork"),
        location: pack_location!("deepwork"),
        allowed_profiles: DEEPWORK_PROFILES,
        required_tools: &[
            "goal_get",
            "plan_get",
            "plan_update",
            "todo_get",
            "todo_update",
        ],
        content_sha256: "14ae721a96c7621e08b2bb030657e4d271f133f92baa6b449a1c687ee1e92284",
        provenance: NATIVE_PROVENANCE,
    },
    BuiltinSkillDescriptor {
        name: "codemap",
        description: "Use the native CodeGraph index and read-only tools to return a scoped structural code map with evidence.",
        content: include_str!("skills/codemap.md"),
        source_id: pack_source_id!("skill/codemap"),
        location: pack_location!("codemap"),
        allowed_profiles: CODEMAP_PROFILES,
        required_tools: &["read", "glob", "grep"],
        content_sha256: "0d52f475ad0dfa6a7d82049ad50b48d39f2201f67d5902ee30375e59a8f34d93",
        provenance: NATIVE_PROVENANCE,
    },
    BuiltinSkillDescriptor {
        name: "verification-planning",
        description: "Plan evidence for high-risk, multi-surface, release, migration, security, or explicitly requested verification work; skip routine bounded changes whose acceptance commands are already clear.",
        content: include_str!("skills/verification-planning.md"),
        source_id: pack_source_id!("skill/verification-planning"),
        location: pack_location!("verification-planning"),
        allowed_profiles: VERIFICATION_PROFILES,
        required_tools: &["read"],
        content_sha256: "29d7b59c4b9b026617c48ddc574e2ad01bc6dd6585b87eef4a648e6c02ac0898",
        provenance: NATIVE_PROVENANCE,
    },
    BuiltinSkillDescriptor {
        name: "reflect",
        description: "Extract bounded, reviewable memory candidates from confirmed outcomes without silently changing code or prompts.",
        content: include_str!("skills/reflect.md"),
        source_id: pack_source_id!("skill/reflect"),
        location: pack_location!("reflect"),
        allowed_profiles: REFLECT_PROFILES,
        required_tools: &["read"],
        content_sha256: "fc492f59ae4d699a855f5f6372eb4822293abfefae4f4c6812f86deec12b8d84",
        provenance: NATIVE_PROVENANCE,
    },
    BuiltinSkillDescriptor {
        name: "worktree",
        description: "Safely inspect, create, use, integrate, and clean up user-authorized Git worktrees without claiming runtime-owned leases.",
        content: include_str!("skills/worktree.md"),
        source_id: pack_source_id!("skill/worktree"),
        location: pack_location!("worktree"),
        allowed_profiles: MUTATING_WORK_PROFILES,
        required_tools: &["read", "shell"],
        content_sha256: "29db03056613a6790bed39a2517b35cfc704be0e7a90054831ae7e23de5cc740",
        provenance: NATIVE_PROVENANCE,
    },
    BuiltinSkillDescriptor {
        name: "git-workflow",
        description: "Use when commits, staging, branches, worktrees, delivery handoff, or a materially dirty repository require Git decisions; skip isolated disposable fixtures and ordinary uncommitted edits.",
        content: include_str!("skills/git-workflow.md"),
        source_id: pack_source_id!("skill/git-workflow"),
        location: pack_location!("git-workflow"),
        allowed_profiles: MUTATING_WORK_PROFILES,
        required_tools: &["read", "shell"],
        content_sha256: "f3be76e43e213c6db9c8f3b6cbd37e7984fd85e5f34c913f3f409cc6e9e9e699",
        provenance: NATIVE_PROVENANCE,
    },
    BuiltinSkillDescriptor {
        name: "github-delivery",
        description: "Plan, inspect, or execute GitHub pull request, Actions, CI, tag, artifact, or release delivery with strict evidence and permissions.",
        content: include_str!("skills/github-delivery.md"),
        source_id: pack_source_id!("skill/github-delivery"),
        location: pack_location!("github-delivery"),
        allowed_profiles: USER_FACING_PROFILES,
        required_tools: &["read"],
        content_sha256: "8767494e8544f2a9cba582da9299699757b62bf213186021c7fc610cdbad830e",
        provenance: CODEX_GITHUB_PROVENANCE,
    },
    BuiltinSkillDescriptor {
        name: "ui-design",
        description: "Review or implement UI with existing-system alignment, interaction and accessibility requirements, and real visual acceptance evidence.",
        content: include_str!("skills/ui-design.md"),
        source_id: pack_source_id!("skill/ui-design"),
        location: pack_location!("ui-design"),
        allowed_profiles: UI_DESIGN_PROFILES,
        required_tools: &["read", "skill"],
        content_sha256: "c7be8a1d626f396f2dd068f6ca4f0cd64565bfc5584dba52e7360f4676afb7c0",
        provenance: NATIVE_PROVENANCE,
    },
    BuiltinSkillDescriptor {
        name: "bedrock-model-capability-review",
        description: "Verify an Amazon Bedrock model capability per model id and region from a cited vendor document or an observed probe, and record it with capability_claim before writing provider configuration.",
        content: include_str!("skills/bedrock-model-capability-review.md"),
        source_id: pack_source_id!("skill/bedrock-model-capability-review"),
        location: pack_location!("bedrock-model-capability-review"),
        allowed_profiles: CAPABILITY_CLAIM_PROFILES,
        required_tools: &["read", "capability_claim"],
        content_sha256: "f56621fa11ffeadd9b3d09131ad3efc6e979101a6e4d2a2ba7b66dcc61b81285",
        provenance: NATIVE_PROVENANCE,
    },
];

const BALANCED_REVIEW_SEATS: [BuiltinCouncilSeatDescriptor; 3] = [
    BuiltinCouncilSeatDescriptor {
        id: "implementation-evidence",
        agent: "explorer",
        instruction: "Inspect the relevant implementation and report concrete evidence, constraints, and unknowns.",
    },
    BuiltinCouncilSeatDescriptor {
        id: "contract-evidence",
        agent: "librarian",
        instruction: "Inspect the relevant documented contracts and compatibility assumptions, then report evidence and gaps.",
    },
    BuiltinCouncilSeatDescriptor {
        id: "decision-review",
        agent: "oracle",
        instruction: "Evaluate tradeoffs, failure modes, and alternatives, then recommend a decision grounded in the available evidence.",
    },
];

/// Every Council preset shipped by the first-party pack.
pub const COUNCILS: [BuiltinCouncilPresetDescriptor; 1] = [BuiltinCouncilPresetDescriptor {
    name: "balanced-review",
    description: "Run implementation, contract, and decision reviewers independently, require two valid seats, and synthesize while preserving dissent.",
    source_id: pack_source_id!("council/balanced-review"),
    seats: &BALANCED_REVIEW_SEATS,
    quorum: 2,
    max_parallel: 3,
    deadline_ms: 180_000,
    synthesis_timeout_ms: 60_000,
    max_retries: 1,
    seat_output_bytes: 16_384,
    synthesis_input_bytes: 32_768,
}];

/// The complete static first-party pack.
pub const PACK: FirstPartyOrchestrationPack = FirstPartyOrchestrationPack {
    id: PACK_ID,
    version: PACK_VERSION,
    skills: &SKILLS,
    councils: &COUNCILS,
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

/// Borrow the stable ordered Council preset catalog.
#[must_use]
pub const fn councils() -> &'static [BuiltinCouncilPresetDescriptor] {
    &COUNCILS
}

/// Find one built-in Skill by its exact name.
#[must_use]
pub fn skill(name: &str) -> Option<&'static BuiltinSkillDescriptor> {
    SKILLS.iter().find(|skill| skill.name == name)
}

/// Find one built-in Council preset by its exact name.
#[must_use]
pub fn council(name: &str) -> Option<&'static BuiltinCouncilPresetDescriptor> {
    COUNCILS.iter().find(|preset| preset.name == name)
}

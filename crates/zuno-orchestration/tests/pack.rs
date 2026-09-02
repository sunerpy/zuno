use std::collections::BTreeSet;

use sha2::{Digest, Sha256};
use zuno_orchestration::{
    COUNCILS, PACK_ID, PACK_VERSION, SKILLS, council, councils, pack, skill, skills,
};

const EXPECTED_NAMES: [&str; 11] = [
    "customize-zuno",
    "develop-zuno",
    "deepwork",
    "codemap",
    "verification-planning",
    "reflect",
    "worktree",
    "git-workflow",
    "github-delivery",
    "ui-design",
    "bedrock-model-capability-review",
];

#[test]
fn pack_has_stable_identity_and_exact_catalog() {
    assert_eq!(pack().id, PACK_ID);
    assert_eq!(pack().version, PACK_VERSION);
    assert_eq!(pack().skills, skills());
    assert_eq!(pack().councils, councils());
    assert_eq!(
        skills().iter().map(|entry| entry.name).collect::<Vec<_>>(),
        EXPECTED_NAMES
    );
    for name in EXPECTED_NAMES {
        assert_eq!(skill(name).map(|entry| entry.name), Some(name));
    }
    assert!(skill("missing").is_none());
    assert_eq!(
        councils()
            .iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>(),
        vec!["balanced-review"]
    );
    assert_eq!(
        council("balanced-review").map(|entry| entry.name),
        Some("balanced-review")
    );
    assert!(council("missing").is_none());
}

#[test]
fn council_presets_are_bounded_and_reference_the_canonical_roster() {
    for preset in COUNCILS {
        assert!(!preset.name.trim().is_empty());
        assert!(!preset.description.trim().is_empty());
        assert!(preset.source_id.contains(PACK_ID));
        assert!(!preset.seats.is_empty());
        assert!(preset.quorum > 0 && preset.quorum <= preset.seats.len());
        assert!(preset.max_parallel > 0 && preset.max_parallel <= preset.seats.len());
        assert!(preset.deadline_ms > 0);
        assert!(preset.synthesis_timeout_ms > 0);
        assert!(preset.synthesis_timeout_ms < preset.deadline_ms);
        assert!(preset.seat_output_bytes > 0);
        assert!(preset.synthesis_input_bytes > 0);
        assert_eq!(
            preset
                .seats
                .iter()
                .map(|seat| seat.id)
                .collect::<BTreeSet<_>>()
                .len(),
            preset.seats.len()
        );
        for seat in preset.seats {
            assert!(!seat.id.trim().is_empty());
            assert!(!seat.instruction.trim().is_empty());
            assert!(["explorer", "librarian", "oracle"].contains(&seat.agent));
        }
    }
}

#[test]
fn names_sources_and_locations_are_unique() {
    let names = SKILLS
        .iter()
        .map(|entry| entry.name)
        .collect::<BTreeSet<_>>();
    let source_ids = SKILLS
        .iter()
        .map(|entry| entry.source_id)
        .collect::<BTreeSet<_>>();
    let locations = SKILLS
        .iter()
        .map(|entry| entry.location)
        .collect::<BTreeSet<_>>();

    assert_eq!(names.len(), SKILLS.len());
    assert_eq!(source_ids.len(), SKILLS.len());
    assert_eq!(locations.len(), SKILLS.len());
}

#[test]
fn metadata_is_complete_and_self_consistent() {
    for entry in SKILLS {
        assert!(!entry.name.trim().is_empty());
        assert!(!entry.description.trim().is_empty());
        assert!(!entry.source_id.trim().is_empty());
        assert!(!entry.location.trim().is_empty());
        assert!(entry.source_id.contains(PACK_ID));
        assert!(entry.source_id.contains(entry.name));
        assert!(entry.source_id.ends_with(PACK_VERSION));
        assert!(entry.location.contains(PACK_ID));
        assert!(entry.location.contains(entry.name));
        assert!(entry.location.contains(PACK_VERSION));
        assert!(!entry.allowed_profiles.is_empty());
        assert!(!entry.required_tools.is_empty());
        assert_eq!(
            entry
                .allowed_profiles
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .len(),
            entry.allowed_profiles.len(),
            "{} has duplicate allowed profiles",
            entry.name
        );
        assert_eq!(
            entry
                .required_tools
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .len(),
            entry.required_tools.len(),
            "{} has duplicate required tools",
            entry.name
        );
        assert!(!entry.provenance.inspiration.trim().is_empty());
        assert!(!entry.provenance.license_review.trim().is_empty());
        assert!(!entry.provenance.upstream_revision.trim().is_empty());
        assert_eq!(entry.content_sha256.len(), 64);
        assert!(
            entry
                .content_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
    }
}

#[test]
fn content_hashes_match_the_embedded_resources() {
    for entry in SKILLS {
        let digest = Sha256::digest(entry.content.as_bytes());
        assert_eq!(
            hex::encode(digest),
            entry.content_sha256,
            "{} content hash changed; review the body and update its descriptor",
            entry.name
        );
    }
}

#[test]
fn bodies_are_nonempty_original_guidance_without_runtime_policy_duplication() {
    for entry in SKILLS {
        assert!(!entry.content.trim().is_empty(), "{} is empty", entry.name);
        assert!(
            !entry
                .content
                .contains("This Skill does not grant tools, permissions"),
            "{} duplicates host-owned runtime authority policy",
            entry.name
        );

        let body = entry.content.to_ascii_lowercase();
        for forbidden in [
            "this skill grants tools",
            "this skill grants permissions",
            "bypass the permission",
            "bypasses the permission",
            "elevate permissions",
            "elevates permissions",
        ] {
            assert!(
                !body.contains(forbidden),
                "{} contains a forbidden authority claim: {forbidden}",
                entry.name
            );
        }
    }
}

#[test]
fn deepwork_declares_every_durable_state_reader_and_writer_it_uses() {
    let deepwork = skill("deepwork").expect("deepwork descriptor");
    assert_eq!(
        deepwork.required_tools,
        &[
            "goal_get",
            "plan_get",
            "plan_update",
            "todo_get",
            "todo_update"
        ]
    );
}

#[test]
fn focused_workflow_skills_stay_within_their_prompt_budgets() {
    for (name, minimum, maximum) in [
        ("deepwork", 80, 110),
        ("git-workflow", 165, 195),
        ("github-delivery", 180, 245),
        ("verification-planning", 80, 105),
        ("bedrock-model-capability-review", 190, 250),
    ] {
        let entry = skill(name).expect("skill descriptor");
        let words = entry.content.split_whitespace().count();
        assert!(
            (minimum..=maximum).contains(&words),
            "{name} has {words} words; expected {minimum}..={maximum}"
        );
    }
}

#[test]
fn worktree_skill_guides_only_authorized_and_safe_lifecycle_operations() {
    let worktree = skill("worktree").expect("worktree descriptor");
    assert!(
        worktree
            .content
            .contains("the user's request explicitly authorizes")
    );
    assert!(worktree.content.contains("`git worktree list --porcelain`"));
    assert!(worktree.content.contains("`git worktree add`"));
    assert!(worktree.content.contains("Never remove a dirty worktree"));
    assert!(
        worktree
            .content
            .contains("does not own leases, quotas, or automatic cleanup")
    );
}

#[test]
fn git_workflow_batches_commit_preparation_instead_of_rechecking_every_hunk() {
    let workflow = skill("git-workflow").expect("git-workflow descriptor");
    for clause in [
        "Classify the complete diff before changing the index",
        "Batch independent inspection commands",
        "Do not re-read unchanged diffs",
        "Run shared repository gates once",
        "Verify each staged commit with one staged-diff review",
    ] {
        assert!(
            workflow.content.contains(clause),
            "git-workflow is missing `{clause}`:\n{}",
            workflow.content
        );
    }
}

#[test]
fn git_workflow_uses_an_overridable_command_scoped_default_identity() {
    let workflow = skill("git-workflow").expect("git-workflow descriptor");
    for clause in [
        "explicit current-user, repository, and selected",
        "`git -c user.name=zuno-agent -c user.email=zuno-agent@firlab.app commit ...`",
        "never modify Git configuration merely for attribution",
        "preserve the existing author unless reset is explicitly requested",
        "`git show --no-patch --format=fuller HEAD`",
    ] {
        assert!(
            workflow.content.contains(clause),
            "git-workflow is missing `{clause}`:\n{}",
            workflow.content
        );
    }
}

#[test]
fn github_delivery_requires_machine_readable_remote_and_release_evidence() {
    let workflow = skill("github-delivery").expect("github-delivery descriptor");
    for clause in [
        "separate authorized side effects",
        "machine-readable state",
        "every required job must actually conclude `success`",
        "full-SHA action pins",
        "separate gated job",
        "`background: true`",
        "`backgroundPurpose: \"remoteObserver\"`",
        "re-query authoritative",
        "run/attempt or ref",
        "built from the exact ref",
        "consumer-facing install",
        "Never convert a planned",
    ] {
        assert!(
            workflow.content.contains(clause),
            "github-delivery is missing `{clause}`:\n{}",
            workflow.content
        );
    }
    assert!(workflow.provenance.inspiration.contains("OpenAI Codex"));
    assert_eq!(workflow.required_tools, &["read"]);
    assert!(workflow.allowed_profiles.contains(&"plan"));
    assert!(workflow.allowed_profiles.contains(&"build"));
}

#[test]
fn bedrock_capability_review_accepts_only_cited_documents_or_observed_probes() {
    let review =
        skill("bedrock-model-capability-review").expect("bedrock capability review descriptor");
    // Compared with whitespace collapsed, so a clause may be checked as one sentence
    // however the Markdown happens to wrap it.
    let body = review
        .content
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    for clause in [
        "before editing a provider model catalog or enabling a provider feature for an Amazon \
         Bedrock model",
        "specific to one model id in one region",
        "A sibling model's documentation is not evidence about this model",
        "Accept exactly two states as evidence",
        "whose response you observed",
        "Record the claim with `capability_claim` before writing the configuration",
        "`documented` with the citation",
        "`probed` with `probeReceiptId`",
        "Probe again after the last change and record the claim again before completing",
        "Never describe an inferred capability as supported",
    ] {
        assert!(
            body.contains(clause),
            "bedrock-model-capability-review is missing `{clause}`:\n{}",
            review.content
        );
    }
    assert_eq!(
        review.required_tools,
        &["read", "capability_claim"],
        "the skill instructs a tool call, so it must not be advertised where the tool is absent"
    );
    assert!(review.allowed_profiles.contains(&"build"));
    assert!(
        !review.allowed_profiles.contains(&"plan"),
        "the skill governs a write, which the read-only planner does not make"
    );
}

#[test]
fn reusable_design_method_is_a_skill_but_product_workflows_remain_user_owned() {
    assert!(skill("dual-review").is_none());
    assert!(skill("auto-release").is_none());
    assert!(skill("github-delivery").is_some());
    let design = skill("ui-design").expect("ui-design descriptor");
    assert!(design.required_tools.contains(&"read"));
    assert!(design.required_tools.contains(&"skill"));
    assert!(design.content.contains("existing design system"));
    assert!(
        design.content.contains("visual evidence"),
        "the design workflow must require runtime evidence rather than source-only confidence"
    );
}

#[test]
fn develop_zuno_is_embedded_guidance_not_a_product_workflow_or_cli_command() {
    let development = skill("develop-zuno").expect("develop-zuno descriptor");
    assert!(development.content.contains("extension.json"));
    assert!(development.content.contains("SKILL.md"));
    assert!(development.content.contains("agent/"));
    assert!(development.content.contains("docs/plugins.md"));
    assert!(development.content.contains("github.com/sunerpy/zuno"));
    assert!(development.content.contains("not a CLI command"));
    assert!(development.content.contains("user-owned"));
    assert!(development.content.contains("static tools"));
    assert!(development.content.contains("Provider transports"));
    assert!(
        development
            .content
            .contains("remain native Rust extension points")
    );
    assert!(
        !development
            .content
            .contains("register tools, hooks, providers")
    );
}

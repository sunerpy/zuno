use std::collections::BTreeSet;

use sha2::{Digest, Sha256};
use zuno_orchestration::{PACK_ID, PACK_VERSION, SKILLS, pack, skill, skills};

const EXPECTED_NAMES: [&str; 7] = [
    "customize-zuno",
    "deepwork",
    "codemap",
    "verification-planning",
    "reflect",
    "worktree",
    "git-workflow",
];

const AUTHORITY_GUARD: &str = "This Skill does not grant tools, permissions, filesystem access, network access,\nor environment access.";

#[test]
fn pack_has_stable_identity_and_exact_catalog() {
    assert_eq!(pack().id, PACK_ID);
    assert_eq!(pack().version, PACK_VERSION);
    assert_eq!(pack().skills, skills());
    assert_eq!(
        skills().iter().map(|entry| entry.name).collect::<Vec<_>>(),
        EXPECTED_NAMES
    );
    for name in EXPECTED_NAMES {
        assert_eq!(skill(name).map(|entry| entry.name), Some(name));
    }
    assert!(skill("missing").is_none());
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
fn bodies_are_nonempty_original_guidance_without_authority_claims() {
    for entry in SKILLS {
        assert!(!entry.content.trim().is_empty(), "{} is empty", entry.name);
        assert!(
            entry.content.contains(AUTHORITY_GUARD),
            "{} lacks the explicit no-authority guard",
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
fn worktree_skill_is_explicitly_preflight_only() {
    let worktree = skill("worktree").expect("worktree descriptor");
    assert!(
        worktree
            .content
            .contains("This Skill performs preflight checks only.")
    );
    assert!(worktree.content.contains("without creating either"));
    assert!(worktree.content.contains("Do not run `git worktree add`"));
    assert!(
        worktree
            .content
            .contains("or claim that Zuno will clean them up")
    );
}

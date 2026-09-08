use crate::store::{FinalizeReview, ObservedClaimEvidence};
use crate::*;
use schemars::schema_for;
use serde_json::json;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use zuno_paths::DbLocation;

const SESSION: &str = "ses_review";

fn fixture() -> (tempfile::TempDir, Arc<zuno_db::Pool>, ReviewStore) {
    let root = tempfile::tempdir().expect("review fixture");
    let location = DbLocation::File(root.path().join("review.db"));
    let mut connection = zuno_db::open::open(&location).expect("connection");
    zuno_db::migration::apply(&mut connection).expect("migration");
    connection
        .execute(
            "INSERT INTO project (id, worktree, vcs, time_created, time_updated, sandboxes) \
             VALUES ('proj', '/tmp/proj', NULL, 1, 1, '[]')",
            (),
        )
        .expect("project");
    let transaction = connection.transaction().expect("session transaction");
    zuno_db::session::create(
        &transaction,
        &zuno_db::session::SessionCreate::new(
            SESSION,
            SESSION,
            "proj",
            "/tmp/proj",
            "/tmp/proj",
            "review fixture",
            "0.0.0",
        )
        .at(1),
    )
    .expect("session");
    transaction.commit().expect("session commit");
    drop(connection);
    let pool = Arc::new(zuno_db::Pool::open(&location).expect("review pool"));
    let store = ReviewStore::new(Arc::clone(&pool));
    (root, pool, store)
}

fn source(id: &str) -> ReviewSourceSnapshot {
    ReviewSourceSnapshot {
        id: id.to_owned(),
        repository_root: "/tmp/proj".to_owned(),
        head_sha: "ec6d4995".to_owned(),
        branch: Some("main".to_owned()),
        worktree_path: "/tmp/proj".to_owned(),
        dirty: false,
        worktree_digest: "sha256:source".to_owned(),
        scope_paths: vec!["src/lib.rs".to_owned()],
        codegraph: CodeGraphIndexSnapshot {
            initialized: true,
            extraction_status: "current".to_owned(),
            built_with_extraction_version: 12,
            current_extraction_version: 12,
            ..CodeGraphIndexSnapshot::default()
        },
        captured_at_ms: 1,
    }
}

fn anchor(digest: &str) -> EvidenceAnchor {
    EvidenceAnchor {
        path: "src/lib.rs".to_owned(),
        symbol: Some("run".to_owned()),
        start_line: Some(1),
        end_line: Some(1),
        content_digest: Some(digest.to_owned()),
        reference: None,
    }
}

fn claim(review: &ReviewReadiness, digest: &str) -> NewReviewClaim {
    NewReviewClaim {
        review_id: review.review_id.clone(),
        statement: "the runtime follows the durable admission path".to_owned(),
        kind: ClaimKind::Fact,
        priority: ClaimPriority::P0,
        layer: Some(SystemLayer::RuntimeScheduling),
        source_snapshot_id: review.source.id.clone(),
        evidence: vec![anchor(digest)],
        counterchecks: Vec::new(),
        asserted_by: ActorRef::Parent,
    }
}

fn delegate_report(snapshot_id: &str, statement: &str) -> DelegationEvidenceReport {
    serde_json::from_value(json!({
        "source_snapshot_id":snapshot_id,
        "scope_checked":["src/lib.rs"],
        "claims":[{
            "statement":statement,
            "kind":"fact",
            "priority":"p1",
            "evidence":[{
                "path":"src/lib.rs",
                "content_digest":"sha256:a"
            }],
            "counterchecks":[]
        }],
        "contradictions":[],
        "unresolved":[],
        "concise_summary":statement
    }))
    .expect("delegate report")
}

fn seed_council(store: &ReviewStore, review: &ReviewReadiness) -> ReviewReadiness {
    let first = store
        .import_report(
            SESSION,
            &review.review_id,
            review.revision,
            "implementation-evidence",
            delegate_report(&review.source.id, "implementation evidence is recorded"),
            11,
        )
        .expect("first seat");
    store
        .import_report(
            SESSION,
            &review.review_id,
            first.revision,
            "contract-evidence",
            delegate_report(&review.source.id, "contract evidence is recorded"),
            12,
        )
        .expect("second seat")
}

#[test]
fn a_ready_review_is_invalidated_by_a_later_contest() {
    let (_root, _pool, store) = fixture();
    let review = store
        .open_review(
            SESSION,
            Some(".zuno/plan.md"),
            None,
            None,
            source("rsnap_1"),
            10,
        )
        .expect("open");
    let review = seed_council(&store, &review);
    let (readiness, recorded) = store
        .record_claim(SESSION, review.revision, claim(&review, "sha256:a"), 20)
        .expect("record");
    let (readiness, verified) = store
        .verify_claim(
            SESSION,
            &review.review_id,
            readiness.revision,
            &recorded.id,
            true,
            30,
        )
        .expect("verify");
    let ready = store
        .finalize(FinalizeReview {
            session_id: SESSION,
            review_id: &review.review_id,
            expected_revision: readiness.revision,
            requested_status: ReviewStatus::Ready,
            load_bearing_claim_ids: std::slice::from_ref(&verified.id),
            extra_blockers: &[],
            observed: &[ObservedClaimEvidence {
                claim_id: verified.id.clone(),
                evidence: Ok(vec![anchor("sha256:a")]),
            }],
            at_ms: 40,
        })
        .expect("finalize");
    assert_eq!(ready.readiness.status, ReviewStatus::Ready);
    assert!(
        ready
            .readiness
            .receipt_id
            .as_deref()
            .is_some_and(|receipt| receipt.starts_with("rrcp_"))
    );

    let (draft, contested) = store
        .contest_claim(
            SESSION,
            &review.review_id,
            ready.readiness.revision,
            &verified.id,
            "a counterexample was supplied",
            50,
        )
        .expect("contest");

    assert_eq!(contested.status, ClaimStatus::Contested);
    assert_eq!(draft.status, ReviewStatus::Draft);
    assert_eq!(draft.receipt_id, None);
    assert_eq!(draft.revision, ready.readiness.revision + 1);
}

#[test]
fn a_changed_anchor_is_marked_stale_inside_finalize() {
    let (_root, _pool, store) = fixture();
    let review = store
        .open_review(SESSION, None, None, None, source("rsnap_1"), 10)
        .expect("open");
    let review = seed_council(&store, &review);
    let (readiness, recorded) = store
        .record_claim(SESSION, review.revision, claim(&review, "sha256:a"), 20)
        .expect("record");
    let (readiness, verified) = store
        .verify_claim(
            SESSION,
            &review.review_id,
            readiness.revision,
            &recorded.id,
            true,
            30,
        )
        .expect("verify");

    let outcome = store
        .finalize(FinalizeReview {
            session_id: SESSION,
            review_id: &review.review_id,
            expected_revision: readiness.revision,
            requested_status: ReviewStatus::Ready,
            load_bearing_claim_ids: std::slice::from_ref(&verified.id),
            extra_blockers: &[],
            observed: &[ObservedClaimEvidence {
                claim_id: verified.id.clone(),
                evidence: Ok(vec![anchor("sha256:b")]),
            }],
            at_ms: 40,
        })
        .expect("finalize");

    assert_eq!(outcome.readiness.status, ReviewStatus::Draft);
    assert_eq!(
        outcome
            .claims
            .iter()
            .find(|claim| claim.id == verified.id)
            .expect("claim")
            .status,
        ClaimStatus::Stale
    );
    assert!(
        outcome
            .blockers
            .iter()
            .any(|blocker| blocker.claim_id.as_deref() == Some(verified.id.as_str()))
    );
}

#[test]
fn imported_reports_persist_claims_contradictions_and_unresolved_questions() {
    let (_root, _pool, store) = fixture();
    let review = store
        .open_review(SESSION, None, None, None, source("rsnap_1"), 10)
        .expect("open");
    let report: DelegationEvidenceReport = serde_json::from_value(json!({
        "source_snapshot_id":"rsnap_1",
        "scope_checked":["src/lib.rs"],
        "claims":[{
            "statement":"the implementation exposes a recovery path",
            "kind":"fact",
            "priority":"p1",
            "evidence":[{
                "path":"src/lib.rs",
                "content_digest":"sha256:a"
            }],
            "counterchecks":[]
        }],
        "contradictions":[{
            "statement":"two paths disagree",
            "conflicting":["path a","path b"]
        }],
        "unresolved":[{
            "question":"which client owns the final projection?",
            "next_check":"inspect the ACP projector"
        }],
        "concise_summary":"one claim and two open issues"
    }))
    .expect("report");

    let readiness = store
        .import_report(
            SESSION,
            &review.review_id,
            review.revision,
            "implementation-evidence",
            report,
            20,
        )
        .expect("import");

    assert_eq!(
        store
            .claims(SESSION, &review.review_id)
            .expect("claims")
            .len(),
        1
    );
    assert_eq!(readiness.issues.len(), 2);
    assert_eq!(readiness.delegate_reports, vec!["implementation-evidence"]);
    assert!(readiness.issues.iter().all(|issue| !issue.resolved));
}

#[test]
fn dirty_scope_digest_changes_when_bytes_change_but_dirty_stays_true() {
    let root = tempfile::tempdir().expect("repository");
    run_git(root.path(), &["init"]);
    run_git(root.path(), &["config", "user.email", "test@example.com"]);
    run_git(root.path(), &["config", "user.name", "Test"]);
    std::fs::create_dir_all(root.path().join("src")).expect("src");
    std::fs::write(root.path().join("src/lib.rs"), "one\n").expect("seed");
    run_git(root.path(), &["add", "src/lib.rs"]);
    run_git(root.path(), &["commit", "-m", "seed"]);

    let probe = RepositoryReviewSourceProbe::new(root.path());
    std::fs::write(root.path().join("src/lib.rs"), "two\n").expect("first edit");
    let first = probe
        .capture(&["src/lib.rs".to_owned()], 10)
        .expect("first capture");
    std::fs::write(root.path().join("src/lib.rs"), "three\n").expect("second edit");
    let second = probe
        .capture(&["src/lib.rs".to_owned()], 20)
        .expect("second capture");

    assert!(first.dirty && second.dirty);
    assert_eq!(first.head_sha, second.head_sha);
    assert_ne!(first.worktree_digest, second.worktree_digest);
}

#[test]
fn trusted_review_fields_are_absent_from_model_input_schemas() {
    let open = serde_json::to_value(schema_for!(ReviewOpenParams)).expect("open schema");
    let claim = serde_json::to_value(schema_for!(ReviewClaimParams)).expect("claim schema");
    let finalize =
        serde_json::to_value(schema_for!(ReviewFinalizeParams)).expect("finalize schema");
    let encoded = format!("{open}{claim}{finalize}");

    for forbidden in [
        "head_sha",
        "headSHA",
        "dirty",
        "codegraph",
        "by_parent",
        "byParent",
        "receipt_id",
        "receiptID",
    ] {
        assert!(
            !encoded.contains(forbidden),
            "trusted field `{forbidden}` leaked into a model-owned schema"
        );
    }
}

fn run_git(root: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

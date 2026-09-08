use crate::store::{FinalizeObservation, FinalizeReview, ObservedClaimEvidence};
use crate::*;
use schemars::schema_for;
use serde_json::json;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use zuno_db::job::{AgentJobStore, JobSettlement, JobSubject, NewAgentJob, ReportDelivery};
use zuno_paths::DbLocation;
use zuno_tool::{AllowAll, NeverInterrupted, ToolContext, TypedTool};

const SESSION: &str = "ses_review";

struct FixedPlanProbe(Option<ReviewPlanBinding>);

impl ReviewPlanProbe for FixedPlanProbe {
    fn current(&self, _session_id: &str) -> Result<Option<ReviewPlanBinding>, String> {
        Ok(self.0.clone())
    }
}

#[derive(Default)]
struct RecordingCouncilRunner(Mutex<Vec<(String, String, String)>>);

#[async_trait::async_trait]
impl ReviewCouncilRunner for RecordingCouncilRunner {
    async fn run(
        &self,
        review: &ReviewReadiness,
        question: String,
        context: ToolContext,
    ) -> Result<serde_json::Value, String> {
        self.0.lock().expect("recording Council").push((
            review.review_id.clone(),
            question,
            context.agent,
        ));
        Ok(json!({"runID":"run_auto","status":"completed"}))
    }
}

struct ChangingAnchorProbe {
    snapshot: ReviewSourceSnapshot,
    anchors: AtomicUsize,
    change_after: usize,
}

impl ReviewSourceProbe for ChangingAnchorProbe {
    fn capture(
        &self,
        scope_paths: &[String],
        _artifact_path: Option<&str>,
        at_ms: i64,
    ) -> Result<ReviewSourceSnapshot, SourceProbeError> {
        let mut snapshot = self.snapshot.clone();
        snapshot.scope_paths = scope_paths.to_vec();
        snapshot.captured_at_ms = at_ms;
        Ok(snapshot)
    }

    fn anchor(&self, requested: &EvidenceAnchor) -> Result<EvidenceAnchor, SourceProbeError> {
        let call = self.anchors.fetch_add(1, Ordering::SeqCst);
        let mut anchored = requested.clone();
        anchored.content_digest = Some(if call < self.change_after {
            "sha256:a".to_owned()
        } else {
            "sha256:b".to_owned()
        });
        Ok(anchored)
    }

    fn anchor_batch(
        &self,
        requested: &[EvidenceAnchor],
        _max_total_bytes: u64,
    ) -> Result<Vec<EvidenceAnchor>, SourceProbeError> {
        requested.iter().map(|anchor| self.anchor(anchor)).collect()
    }
}

struct PostCommitFailureProbe {
    snapshot: ReviewSourceSnapshot,
    captures: AtomicUsize,
}

impl ReviewSourceProbe for PostCommitFailureProbe {
    fn capture(
        &self,
        scope_paths: &[String],
        _artifact_path: Option<&str>,
        at_ms: i64,
    ) -> Result<ReviewSourceSnapshot, SourceProbeError> {
        if self.captures.fetch_add(1, Ordering::SeqCst) >= 2 {
            return Err(SourceProbeError::Command {
                command: "fixture".to_owned(),
                detail: "x".repeat(32 * 1_024),
            });
        }
        let mut snapshot = self.snapshot.clone();
        snapshot.scope_paths = scope_paths.to_vec();
        snapshot.captured_at_ms = at_ms;
        Ok(snapshot)
    }

    fn anchor(&self, requested: &EvidenceAnchor) -> Result<EvidenceAnchor, SourceProbeError> {
        let mut anchored = requested.clone();
        anchored.content_digest = Some("sha256:a".to_owned());
        Ok(anchored)
    }

    fn anchor_batch(
        &self,
        requested: &[EvidenceAnchor],
        _max_total_bytes: u64,
    ) -> Result<Vec<EvidenceAnchor>, SourceProbeError> {
        requested.iter().map(|anchor| self.anchor(anchor)).collect()
    }
}

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
        artifact: None,
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

fn verification(at_ms: i64) -> ReviewVerificationReceipt {
    ReviewVerificationReceipt {
        session_id: SESSION.to_owned(),
        message_id: "msg_review".to_owned(),
        call_id: format!("call_{at_ms}"),
        agent: "review".to_owned(),
        time_verified: at_ms,
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

fn delegate_receipt(
    report: &DelegationEvidenceReport,
    seat_id: &str,
    agent: &str,
    at_ms: i64,
) -> ReviewDelegateReceipt {
    delegate_receipt_for(report, "run_balanced", seat_id, agent, at_ms)
}

fn delegate_receipt_for(
    report: &DelegationEvidenceReport,
    run_id: &str,
    seat_id: &str,
    agent: &str,
    at_ms: i64,
) -> ReviewDelegateReceipt {
    ReviewDelegateReceipt {
        run_id: run_id.to_owned(),
        job_id: format!("job_{run_id}"),
        preset: "balanced-review".to_owned(),
        preset_source_id: "builtin://balanced-review".to_owned(),
        seat_id: seat_id.to_owned(),
        agent: agent.to_owned(),
        source_snapshot_id: report.source_snapshot_id.clone(),
        report_digest: delegation_report_digest(report),
        time_imported: at_ms,
    }
}

fn open(store: &ReviewStore, snapshot: ReviewSourceSnapshot) -> ReviewReadiness {
    store
        .open_review(SESSION, None, None, true, || Ok(snapshot), 10)
        .expect("open")
}

fn open_for_plan(
    store: &ReviewStore,
    snapshot: ReviewSourceSnapshot,
    plan_id: &str,
    plan_revision: i64,
    at_ms: i64,
) -> ReviewReadiness {
    store
        .open_review(
            SESSION,
            Some(plan_id),
            Some(plan_revision),
            true,
            || Ok(snapshot),
            at_ms,
        )
        .expect("open Plan review")
}

fn seed_completed_council_job(
    store: &ReviewStore,
    run_id: &str,
    seats: &[(&str, &str, &DelegationEvidenceReport)],
    at_ms: i64,
) {
    let jobs = AgentJobStore::new(store.pool());
    let job_id = format!("job_{run_id}");
    jobs.create(NewAgentJob::new(
        &job_id,
        SESSION,
        JobSubject::workflow(run_id, "council:balanced-review"),
        ReportDelivery::Quiet,
        at_ms,
    ))
    .expect("Council job");
    jobs.settle(
        &job_id,
        JobSettlement::completed(
            json!({
                "runID": run_id,
                "preset": "balanced-review",
                "status": "completed",
                "seats": seats
                    .iter()
                    .map(|(id, agent, report)| json!({
                        "id": id,
                        "agent": agent,
                        "status": "completed",
                        "report": report,
                    }))
                    .collect::<Vec<_>>(),
            }),
            at_ms + 1,
            None,
        ),
    )
    .expect("complete Council job");
}

fn seed_failed_council_job(store: &ReviewStore, run_id: &str, at_ms: i64) {
    let jobs = AgentJobStore::new(store.pool());
    let job_id = format!("job_{run_id}");
    jobs.create(NewAgentJob::new(
        &job_id,
        SESSION,
        JobSubject::workflow(run_id, "council:balanced-review"),
        ReportDelivery::Quiet,
        at_ms,
    ))
    .expect("Council job");
    jobs.settle(
        &job_id,
        JobSettlement::failed("synthesis failed", at_ms + 1, None).with_result(json!({
            "runID": run_id,
            "preset": "balanced-review",
            "status": "failed",
        })),
    )
    .expect("fail Council job");
}

fn seed_council(store: &ReviewStore, review: &ReviewReadiness) -> ReviewReadiness {
    let first_report = delegate_report(&review.source.id, "implementation evidence is recorded");
    let second_report = delegate_report(&review.source.id, "contract evidence is recorded");
    seed_completed_council_job(
        store,
        "run_balanced",
        &[
            ("implementation-evidence", "explorer", &first_report),
            ("contract-evidence", "oracle", &second_report),
        ],
        8,
    );
    let first = store
        .import_report(
            SESSION,
            &review.review_id,
            review.revision,
            delegate_receipt(&first_report, "implementation-evidence", "explorer", 11),
            first_report,
            11,
        )
        .expect("first seat");
    store
        .import_report(
            SESSION,
            &review.review_id,
            first.revision,
            delegate_receipt(&second_report, "contract-evidence", "oracle", 12),
            second_report,
            12,
        )
        .expect("second seat")
}

fn finalize_one_verified_claim(
    store: &ReviewStore,
    review: &ReviewReadiness,
) -> ReviewFinalizeOutcome {
    let (readiness, recorded) = store
        .record_claim(SESSION, review.revision, claim(review, "sha256:a"), 20)
        .expect("record");
    let (readiness, verified) = store
        .verify_claim(
            SESSION,
            &review.review_id,
            readiness.revision,
            &recorded.id,
            verification(30),
            30,
        )
        .expect("verify");
    store
        .finalize(
            FinalizeReview {
                session_id: SESSION,
                review_id: &review.review_id,
                expected_revision: readiness.revision,
                requested_status: ReviewStatus::Ready,
                commit_ready: true,
                load_bearing_claim_ids: std::slice::from_ref(&verified.id),
                extra_blockers: &[],
                at_ms: 40,
            },
            |readiness, _| {
                Ok(FinalizeObservation {
                    observed: vec![ObservedClaimEvidence {
                        claim_id: verified.id.clone(),
                        evidence: Ok(vec![anchor("sha256:a")]),
                    }],
                    current_source: readiness.source.clone(),
                    plan_current: true,
                    source_changed_during_verification: false,
                })
            },
        )
        .expect("finalize")
}

#[test]
fn plan_review_gate_in_reads_unbound_and_exact_revision_from_a_caller_transaction() {
    let (_root, pool, store) = fixture();
    let review = open_for_plan(&store, source("rsnap_plan_2"), "plan_alpha", 2, 10);
    let mut connection = pool.get().expect("connection");
    let transaction =
        rusqlite::Connection::transaction(&mut connection).expect("caller transaction");

    assert_eq!(
        ReviewStore::plan_review_gate_in(&transaction, SESSION, "plan_alpha", 1)
            .expect("unbound gate"),
        PlanReviewGate::Unbound
    );
    assert_eq!(
        ReviewStore::plan_review_gate_in(&transaction, SESSION, "plan_alpha", 2)
            .expect("draft gate"),
        PlanReviewGate::Draft {
            review_id: review.review_id,
            review_revision: review.revision,
        }
    );
}

#[test]
fn plan_review_gate_reports_ready_with_the_reconstructed_review_revision() {
    let (_root, _pool, store) = fixture();
    let review = open_for_plan(&store, source("rsnap_ready_plan"), "plan_ready", 7, 10);
    let review = seed_council(&store, &review);
    let ready = finalize_one_verified_claim(&store, &review).readiness;

    assert_eq!(
        store
            .plan_review_gate(SESSION, "plan_ready", 7)
            .expect("ready gate"),
        PlanReviewGate::Ready {
            review_id: ready.review_id,
            review_revision: ready.revision,
        }
    );
}

#[test]
fn plan_review_gate_uses_the_newest_binding_sequence_not_later_old_review_activity() {
    let (_root, _pool, store) = fixture();
    let first = open_for_plan(&store, source("rsnap_first"), "plan_shared", 3, 10);
    let second = open_for_plan(&store, source("rsnap_second"), "plan_shared", 3, 20);
    let (changed_first, _) = store
        .record_claim(SESSION, first.revision, claim(&first, "sha256:a"), 30)
        .expect("later activity on older review");
    assert!(changed_first.revision > second.revision);

    assert_eq!(
        store
            .plan_review_gate(SESSION, "plan_shared", 3)
            .expect("latest bound review"),
        PlanReviewGate::Draft {
            review_id: second.review_id,
            review_revision: second.revision,
        }
    );
}

#[test]
fn a_ready_review_is_invalidated_by_a_later_contest() {
    let (_root, _pool, store) = fixture();
    let review = open(&store, source("rsnap_1"));
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
            verification(30),
            30,
        )
        .expect("verify");
    let ready = store
        .finalize(
            FinalizeReview {
                session_id: SESSION,
                review_id: &review.review_id,
                expected_revision: readiness.revision,
                requested_status: ReviewStatus::Ready,
                commit_ready: true,
                load_bearing_claim_ids: std::slice::from_ref(&verified.id),
                extra_blockers: &[],
                at_ms: 40,
            },
            |readiness, _| {
                Ok(FinalizeObservation {
                    observed: vec![ObservedClaimEvidence {
                        claim_id: verified.id.clone(),
                        evidence: Ok(vec![anchor("sha256:a")]),
                    }],
                    current_source: readiness.source.clone(),
                    plan_current: true,
                    source_changed_during_verification: false,
                })
            },
        )
        .expect("finalize");
    assert_eq!(ready.readiness.status, ReviewStatus::Ready);
    assert!(
        ready
            .readiness
            .receipt
            .as_ref()
            .is_some_and(|receipt| receipt.id.starts_with("rrcp_"))
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
    assert_eq!(draft.receipt, None);
    assert_eq!(draft.revision, ready.readiness.revision + 1);
}

#[test]
fn a_changed_anchor_is_marked_stale_inside_finalize() {
    let (_root, _pool, store) = fixture();
    let review = open(&store, source("rsnap_1"));
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
            verification(30),
            30,
        )
        .expect("verify");

    let outcome = store
        .finalize(
            FinalizeReview {
                session_id: SESSION,
                review_id: &review.review_id,
                expected_revision: readiness.revision,
                requested_status: ReviewStatus::Ready,
                commit_ready: true,
                load_bearing_claim_ids: std::slice::from_ref(&verified.id),
                extra_blockers: &[],
                at_ms: 40,
            },
            |readiness, _| {
                Ok(FinalizeObservation {
                    observed: vec![ObservedClaimEvidence {
                        claim_id: verified.id.clone(),
                        evidence: Ok(vec![anchor("sha256:b")]),
                    }],
                    current_source: readiness.source.clone(),
                    plan_current: true,
                    source_changed_during_verification: false,
                })
            },
        )
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

#[tokio::test]
async fn finalize_rechecks_ignored_anchors_before_and_after_commit() {
    for (change_after, expected_status) in [
        (1, ReviewStatus::Draft),
        (2, ReviewStatus::Draft),
        (usize::MAX, ReviewStatus::Ready),
    ] {
        let (_root, _pool, store) = fixture();
        let review = seed_council(
            &store,
            &open(&store, source(&format!("rsnap_race_{change_after}"))),
        );
        let (readiness, recorded) = store
            .record_claim(SESSION, review.revision, claim(&review, "sha256:a"), 20)
            .expect("record");
        let (readiness, verified) = store
            .verify_claim(
                SESSION,
                &review.review_id,
                readiness.revision,
                &recorded.id,
                verification(30),
                30,
            )
            .expect("verify");
        let service = Arc::new(ReviewService::new(
            Arc::new(store.clone()),
            Arc::new(ChangingAnchorProbe {
                snapshot: review.source.clone(),
                anchors: AtomicUsize::new(0),
                change_after,
            }),
        ));
        let output = ReviewFinalizeTool::new(service, Arc::new(NoReviewPlanProbe))
            .run(
                ReviewFinalizeParams {
                    review_id: review.review_id.clone(),
                    expected_revision: readiness.revision,
                    requested_status: ReviewStatus::Ready,
                    load_bearing_claim_ids: vec![verified.id.clone()],
                    blockers: Vec::new(),
                },
                ToolContext::new(
                    SESSION,
                    "msg_finalize",
                    "call_finalize",
                    "review",
                    Arc::new(AllowAll),
                    Arc::new(NeverInterrupted),
                ),
            )
            .await
            .expect("finalize");
        let output: serde_json::Value =
            serde_json::from_str(&output.output).expect("finalize output");
        assert_eq!(
            output["review"]["status"],
            serde_json::Value::String(expected_status.to_string())
        );
        let claims = store.claims(SESSION, &review.review_id).expect("claims");
        let claim = claims
            .iter()
            .find(|claim| claim.id == verified.id)
            .expect("claim");
        if expected_status == ReviewStatus::Draft {
            assert_eq!(claim.status, ClaimStatus::Stale);
        } else {
            assert_eq!(claim.status, ClaimStatus::Verified);
            let readiness = store
                .review(SESSION, &review.review_id)
                .expect("review")
                .expect("readiness");
            assert!(
                readiness
                    .receipt
                    .as_ref()
                    .is_some_and(|receipt| !receipt.evidence_digest.is_empty())
            );
        }
    }
}

#[tokio::test]
async fn post_commit_verification_failure_leaves_no_ready_receipt() {
    let (_root, _pool, store) = fixture();
    let review = seed_council(&store, &open(&store, source("rsnap_post_commit_failure")));
    let (readiness, recorded) = store
        .record_claim(SESSION, review.revision, claim(&review, "sha256:a"), 20)
        .expect("record");
    let (readiness, verified) = store
        .verify_claim(
            SESSION,
            &review.review_id,
            readiness.revision,
            &recorded.id,
            verification(30),
            30,
        )
        .expect("verify");
    let service = Arc::new(ReviewService::new(
        Arc::new(store.clone()),
        Arc::new(PostCommitFailureProbe {
            snapshot: review.source.clone(),
            captures: AtomicUsize::new(0),
        }),
    ));
    let error = ReviewFinalizeTool::new(service, Arc::new(NoReviewPlanProbe))
        .run(
            ReviewFinalizeParams {
                review_id: review.review_id.clone(),
                expected_revision: readiness.revision,
                requested_status: ReviewStatus::Ready,
                load_bearing_claim_ids: vec![verified.id],
                blockers: Vec::new(),
            },
            ToolContext::new(
                SESSION,
                "msg_finalize",
                "call_finalize",
                "review",
                Arc::new(AllowAll),
                Arc::new(NeverInterrupted),
            ),
        )
        .await
        .expect_err("post-commit verification failure");
    let rendered = zuno_error::source::describe(&error);
    assert!(rendered.chars().count() < 1_500, "{rendered}");
    let persisted = store
        .review(SESSION, &review.review_id)
        .expect("review")
        .expect("persisted review");
    assert_eq!(persisted.status, ReviewStatus::Draft);
    assert_eq!(persisted.receipt, None);
}

#[test]
fn persisted_system_blockers_are_bounded() {
    let (_root, _pool, store) = fixture();
    let review = open(&store, source("rsnap_blocker_bound"));
    let readiness = store
        .record_system_blocker(
            SESSION,
            &review.review_id,
            review.revision,
            &"x".repeat(32 * 1_024),
            20,
        )
        .expect("bounded blocker");
    assert_eq!(readiness.blockers.len(), 1);
    assert!(readiness.blockers[0].reason.chars().count() <= 500);
}

#[test]
fn a_refuted_claim_cannot_be_reverified_into_parent_authority() {
    let (_root, _pool, store) = fixture();
    let review = open(&store, source("rsnap_1"));
    let (readiness, recorded) = store
        .record_claim(SESSION, review.revision, claim(&review, "sha256:a"), 20)
        .expect("record");
    let (readiness, refuted) = store
        .refute_claim(
            SESSION,
            &review.review_id,
            readiness.revision,
            &recorded.id,
            "counterexample",
            30,
        )
        .expect("refute");
    assert_eq!(refuted.status, ClaimStatus::Refuted);
    assert!(refuted.parent_verification.is_none());

    let error = store
        .verify_claim(
            SESSION,
            &review.review_id,
            readiness.revision,
            &recorded.id,
            verification(40),
            40,
        )
        .expect_err("refuted claim must require a new claim");
    assert!(matches!(
        error,
        ReviewError::InvalidClaimTransition {
            status: ClaimStatus::Refuted,
            ..
        }
    ));
}

#[test]
fn ready_requires_one_authoritative_balanced_run_and_factual_load_bearing_evidence() {
    let (_root, _pool, store) = fixture();
    let review = open(&store, source("rsnap_1"));
    let first_report = delegate_report(&review.source.id, "first run evidence");
    let second_report = delegate_report(&review.source.id, "second run evidence");
    seed_completed_council_job(
        &store,
        "run_one",
        &[("implementation", "explorer", &first_report)],
        7,
    );
    seed_completed_council_job(
        &store,
        "run_two",
        &[("contract", "oracle", &second_report)],
        8,
    );
    let first = store
        .import_report(
            SESSION,
            &review.review_id,
            review.revision,
            delegate_receipt_for(&first_report, "run_one", "implementation", "explorer", 11),
            first_report,
            11,
        )
        .expect("first run");
    let review = store
        .import_report(
            SESSION,
            &review.review_id,
            first.revision,
            delegate_receipt_for(&second_report, "run_two", "contract", "oracle", 12),
            second_report,
            12,
        )
        .expect("second run");
    let (readiness, recorded) = store
        .record_claim(SESSION, review.revision, claim(&review, "sha256:a"), 20)
        .expect("record");
    let (readiness, verified) = store
        .verify_claim(
            SESSION,
            &review.review_id,
            readiness.revision,
            &recorded.id,
            verification(30),
            30,
        )
        .expect("verify");
    let outcome = store
        .finalize(
            FinalizeReview {
                session_id: SESSION,
                review_id: &review.review_id,
                expected_revision: readiness.revision,
                requested_status: ReviewStatus::Ready,
                commit_ready: true,
                load_bearing_claim_ids: std::slice::from_ref(&verified.id),
                extra_blockers: &[],
                at_ms: 40,
            },
            |readiness, _| {
                Ok(FinalizeObservation {
                    observed: vec![ObservedClaimEvidence {
                        claim_id: verified.id.clone(),
                        evidence: Ok(vec![anchor("sha256:a")]),
                    }],
                    current_source: readiness.source.clone(),
                    plan_current: true,
                    source_changed_during_verification: false,
                })
            },
        )
        .expect("finalize");
    assert_eq!(outcome.readiness.status, ReviewStatus::Draft);
    assert!(
        outcome
            .blockers
            .iter()
            .any(|blocker| blocker.reason.contains("at least 2"))
    );

    let (_root, _pool, store) = fixture();
    let review = seed_council(&store, &open(&store, source("rsnap_2")));
    let recommendation = NewReviewClaim {
        review_id: review.review_id.clone(),
        statement: "ship after documentation review".to_owned(),
        kind: ClaimKind::Recommendation,
        priority: ClaimPriority::P0,
        layer: None,
        source_snapshot_id: review.source.id.clone(),
        evidence: Vec::new(),
        counterchecks: Vec::new(),
        asserted_by: ActorRef::Parent,
    };
    let (readiness, recorded) = store
        .record_claim(SESSION, review.revision, recommendation, 20)
        .expect("recommendation");
    let (readiness, verified) = store
        .verify_claim(
            SESSION,
            &review.review_id,
            readiness.revision,
            &recorded.id,
            verification(30),
            30,
        )
        .expect("verify recommendation");
    let outcome = store
        .finalize(
            FinalizeReview {
                session_id: SESSION,
                review_id: &review.review_id,
                expected_revision: readiness.revision,
                requested_status: ReviewStatus::Ready,
                commit_ready: true,
                load_bearing_claim_ids: std::slice::from_ref(&verified.id),
                extra_blockers: &[],
                at_ms: 40,
            },
            |readiness, _| {
                Ok(FinalizeObservation {
                    observed: vec![ObservedClaimEvidence {
                        claim_id: verified.id.clone(),
                        evidence: Ok(Vec::new()),
                    }],
                    current_source: readiness.source.clone(),
                    plan_current: true,
                    source_changed_during_verification: false,
                })
            },
        )
        .expect("finalize recommendation");
    assert_eq!(outcome.readiness.status, ReviewStatus::Draft);
    assert!(
        outcome
            .blockers
            .iter()
            .any(|blocker| blocker.reason.contains("evidence-bearing factual claim"))
    );
}

#[test]
fn receipts_from_a_failed_durable_council_job_cannot_make_ready() {
    let (_root, _pool, store) = fixture();
    let review = open(&store, source("rsnap_failed"));
    seed_failed_council_job(&store, "run_failed", 8);
    let first_report = delegate_report(&review.source.id, "first validated seat");
    let first = store
        .import_report(
            SESSION,
            &review.review_id,
            review.revision,
            delegate_receipt_for(&first_report, "run_failed", "first", "explorer", 11),
            first_report,
            11,
        )
        .expect("first seat");
    let second_report = delegate_report(&review.source.id, "second validated seat");
    let review = store
        .import_report(
            SESSION,
            &review.review_id,
            first.revision,
            delegate_receipt_for(&second_report, "run_failed", "second", "oracle", 12),
            second_report,
            12,
        )
        .expect("second seat");
    let (readiness, recorded) = store
        .record_claim(SESSION, review.revision, claim(&review, "sha256:a"), 20)
        .expect("record");
    let (readiness, verified) = store
        .verify_claim(
            SESSION,
            &review.review_id,
            readiness.revision,
            &recorded.id,
            verification(30),
            30,
        )
        .expect("verify");
    let outcome = store
        .finalize(
            FinalizeReview {
                session_id: SESSION,
                review_id: &review.review_id,
                expected_revision: readiness.revision,
                requested_status: ReviewStatus::Ready,
                commit_ready: true,
                load_bearing_claim_ids: std::slice::from_ref(&verified.id),
                extra_blockers: &[],
                at_ms: 40,
            },
            |readiness, _| {
                Ok(FinalizeObservation {
                    observed: vec![ObservedClaimEvidence {
                        claim_id: verified.id.clone(),
                        evidence: Ok(vec![anchor("sha256:a")]),
                    }],
                    current_source: readiness.source.clone(),
                    plan_current: true,
                    source_changed_during_verification: false,
                })
            },
        )
        .expect("finalize");
    assert_eq!(outcome.readiness.status, ReviewStatus::Draft);
    assert!(
        outcome
            .blockers
            .iter()
            .any(|blocker| blocker.reason.contains("at least 2"))
    );
}

#[test]
fn receipts_missing_from_the_completed_job_seats_cannot_make_ready() {
    let (_root, _pool, store) = fixture();
    let review = open(&store, source("rsnap_missing_seats"));
    let first_report = delegate_report(&review.source.id, "first claimed seat");
    let second_report = delegate_report(&review.source.id, "second claimed seat");
    seed_completed_council_job(&store, "run_missing_seats", &[], 8);
    let first = store
        .import_report(
            SESSION,
            &review.review_id,
            review.revision,
            delegate_receipt_for(&first_report, "run_missing_seats", "first", "explorer", 11),
            first_report,
            11,
        )
        .expect("first forged seat receipt");
    let review = store
        .import_report(
            SESSION,
            &review.review_id,
            first.revision,
            delegate_receipt_for(&second_report, "run_missing_seats", "second", "oracle", 12),
            second_report,
            12,
        )
        .expect("second forged seat receipt");

    let outcome = finalize_one_verified_claim(&store, &review);
    assert_eq!(outcome.readiness.status, ReviewStatus::Draft);
    assert!(
        outcome
            .blockers
            .iter()
            .any(|blocker| blocker.reason.contains("at least 2"))
    );
}

#[test]
fn imported_reports_persist_claims_contradictions_and_unresolved_questions() {
    let (_root, _pool, store) = fixture();
    let review = open(&store, source("rsnap_1"));
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

    let receipt = delegate_receipt(&report, "implementation-evidence", "explorer", 20);
    let readiness = store
        .import_report(
            SESSION,
            &review.review_id,
            review.revision,
            receipt,
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
    assert_eq!(
        readiness.delegate_reports[0].seat_id,
        "implementation-evidence"
    );
    assert!(readiness.issues.iter().all(|issue| !issue.resolved));
}

#[test]
fn forged_or_mismatched_council_receipts_are_rejected_before_import() {
    let (_root, _pool, store) = fixture();
    let review = open(&store, source("rsnap_1"));
    let report = delegate_report(&review.source.id, "validated report");
    let mut receipt = delegate_receipt(&report, "implementation", "explorer", 20);
    receipt.preset = "invented-review".to_owned();
    let error = store
        .import_report(
            SESSION,
            &review.review_id,
            review.revision,
            receipt,
            report.clone(),
            20,
        )
        .expect_err("wrong preset");
    assert!(matches!(error, ReviewError::InvalidCouncilReceipt(_)));

    let mut receipt = delegate_receipt(&report, "implementation", "explorer", 21);
    receipt.report_digest = "sha256:forged".to_owned();
    let error = store
        .import_report(
            SESSION,
            &review.review_id,
            review.revision,
            receipt,
            report,
            21,
        )
        .expect_err("forged digest");
    assert!(matches!(error, ReviewError::InvalidCouncilReceipt(_)));
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
        .capture(&["src/lib.rs".to_owned()], None, 10)
        .expect("first capture");
    std::fs::write(root.path().join("src/lib.rs"), "three\n").expect("second edit");
    let second = probe
        .capture(&["src/lib.rs".to_owned()], None, 20)
        .expect("second capture");

    assert!(first.dirty && second.dirty);
    assert_eq!(first.head_sha, second.head_sha);
    assert_ne!(first.worktree_digest, second.worktree_digest);
}

#[cfg(unix)]
#[test]
fn scope_pathspec_magic_is_literal_and_untracked_symlinks_are_refused() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("repository");
    run_git(root.path(), &["init"]);
    run_git(root.path(), &["config", "user.email", "test@example.com"]);
    run_git(root.path(), &["config", "user.name", "Test"]);
    std::fs::write(root.path().join("seed"), "seed\n").expect("seed");
    run_git(root.path(), &["add", "seed"]);
    run_git(root.path(), &["commit", "-m", "seed"]);

    let magic_name = ":(exclude)src";
    std::fs::write(root.path().join(magic_name), "literal\n").expect("literal path");
    let probe = RepositoryReviewSourceProbe::new(root.path());
    let captured = probe
        .capture(&[magic_name.to_owned()], None, 10)
        .expect("literal pathspec");
    assert!(captured.dirty);
    assert_eq!(captured.scope_paths, vec![magic_name]);

    symlink("/etc/passwd", root.path().join("escape")).expect("symlink");
    let error = probe
        .capture(&["escape".to_owned()], None, 20)
        .expect_err("untracked symlink must not be followed");
    assert!(matches!(error, SourceProbeError::Symlink(path) if path == "escape"));
}

#[cfg(unix)]
#[test]
fn evidence_cannot_escape_through_an_ancestor_symlink() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("repository");
    let outside = tempfile::tempdir().expect("outside");
    std::fs::write(outside.path().join("secret"), "outside\n").expect("outside file");
    symlink(outside.path(), root.path().join("linked")).expect("ancestor symlink");

    let error = RepositoryReviewSourceProbe::new(root.path())
        .anchor(&EvidenceAnchor {
            path: "linked/secret".to_owned(),
            symbol: None,
            start_line: None,
            end_line: None,
            content_digest: None,
            reference: None,
        })
        .expect_err("capability-relative open must not escape through an ancestor symlink");
    assert!(matches!(
        error,
        SourceProbeError::Read { .. }
            | SourceProbeError::Symlink(_)
            | SourceProbeError::OutsideRepository(_)
    ));
}

#[test]
fn evidence_reads_stop_at_the_file_byte_limit() {
    let root = tempfile::tempdir().expect("repository");
    std::fs::write(root.path().join("large"), vec![b'x'; 4 * 1_024 * 1_024 + 1])
        .expect("large source");

    let error = RepositoryReviewSourceProbe::new(root.path())
        .anchor(&EvidenceAnchor {
            path: "large".to_owned(),
            symbol: None,
            start_line: None,
            end_line: None,
            content_digest: None,
            reference: None,
        })
        .expect_err("oversized evidence must be bounded");
    assert!(matches!(error, SourceProbeError::FileTooLarge { .. }));
}

#[test]
fn evidence_batches_deduplicate_files_and_enforce_one_total_budget() {
    let root = tempfile::tempdir().expect("repository");
    std::fs::write(root.path().join("shared"), vec![b'a'; 1_024 * 1_024]).expect("shared evidence");
    std::fs::write(root.path().join("other"), vec![b'b'; 1_024 * 1_024]).expect("other evidence");
    let probe = RepositoryReviewSourceProbe::new(root.path());
    let shared = EvidenceAnchor {
        path: "shared".to_owned(),
        symbol: None,
        start_line: None,
        end_line: None,
        content_digest: None,
        reference: None,
    };
    let repeated = vec![shared.clone(); 64];
    let anchored = probe
        .anchor_batch(&repeated, 1_024 * 1_024)
        .expect("one file is read once");
    assert_eq!(anchored.len(), repeated.len());

    let error = probe
        .anchor_batch(
            &[
                shared,
                EvidenceAnchor {
                    path: "other".to_owned(),
                    symbol: None,
                    start_line: None,
                    end_line: None,
                    content_digest: None,
                    reference: None,
                },
            ],
            1_024 * 1_024,
        )
        .expect_err("two unique files exceed the batch budget");
    assert!(matches!(error, SourceProbeError::EvidenceTooLarge { .. }));
}

#[test]
fn evidence_end_line_must_exist_in_the_file() {
    let root = tempfile::tempdir().expect("repository");
    run_git(root.path(), &["init"]);
    run_git(root.path(), &["config", "user.email", "test@example.com"]);
    run_git(root.path(), &["config", "user.name", "Test"]);
    std::fs::write(root.path().join("source.rs"), "one\n").expect("source");
    run_git(root.path(), &["add", "source.rs"]);
    run_git(root.path(), &["commit", "-m", "seed"]);

    let error = RepositoryReviewSourceProbe::new(root.path())
        .anchor(&EvidenceAnchor {
            path: "source.rs".to_owned(),
            symbol: None,
            start_line: Some(1),
            end_line: Some(2),
            content_digest: None,
            reference: None,
        })
        .expect_err("range past EOF");
    assert!(matches!(
        error,
        SourceProbeError::RangeEndOutsideFile {
            end: 2,
            lines: 1,
            ..
        }
    ));
}

#[tokio::test]
async fn review_get_invalidates_ready_when_an_ignored_artifact_changes() {
    let repository = tempfile::tempdir().expect("repository");
    run_git(repository.path(), &["init"]);
    run_git(
        repository.path(),
        &["config", "user.email", "test@example.com"],
    );
    run_git(repository.path(), &["config", "user.name", "Test"]);
    std::fs::create_dir_all(repository.path().join("src")).expect("src");
    std::fs::write(repository.path().join("src/lib.rs"), "pub fn run() {}\n").expect("source");
    std::fs::write(repository.path().join(".gitignore"), ".zuno/\n").expect("gitignore");
    run_git(repository.path(), &["add", "src/lib.rs", ".gitignore"]);
    run_git(repository.path(), &["commit", "-m", "seed"]);
    std::fs::create_dir_all(repository.path().join(".zuno")).expect("artifact directory");
    std::fs::write(repository.path().join(".zuno/plan.md"), "first plan\n").expect("artifact");

    let (_root, _pool, store) = fixture();
    let service = Arc::new(ReviewService::new(
        Arc::new(store.clone()),
        Arc::new(RepositoryReviewSourceProbe::new(repository.path())),
    ));
    let plans: Arc<dyn ReviewPlanProbe> = Arc::new(NoReviewPlanProbe);
    let open_tool = ReviewOpenTool::new(
        Arc::clone(&service),
        Arc::clone(&plans),
        Arc::new(NoopReviewCouncilRunner),
    );
    let opened = open_tool
        .run(
            ReviewOpenParams {
                artifact_path: Some(".zuno/plan.md".to_owned()),
                scope_paths: vec!["src/lib.rs".to_owned()],
                plan_id: None,
                plan_revision: None,
            },
            ToolContext::new(
                SESSION,
                "msg_open",
                "call_open",
                "review",
                Arc::new(AllowAll),
                Arc::new(NeverInterrupted),
            ),
        )
        .await
        .expect("open review");
    let opened: serde_json::Value = serde_json::from_str(&opened.output).expect("open output JSON");
    let review_id = opened["review"]["reviewID"]
        .as_str()
        .expect("review id")
        .to_owned();
    let review = store
        .review(SESSION, &review_id)
        .expect("review")
        .expect("review exists");
    let review = seed_council(&store, &review);
    let source = service.source();
    let anchored = source
        .anchor(&EvidenceAnchor {
            path: "src/lib.rs".to_owned(),
            symbol: Some("run".to_owned()),
            start_line: Some(1),
            end_line: Some(1),
            content_digest: None,
            reference: None,
        })
        .expect("anchor source");
    let (readiness, recorded) = store
        .record_claim(
            SESSION,
            review.revision,
            NewReviewClaim {
                review_id: review_id.clone(),
                statement: "the source defines run".to_owned(),
                kind: ClaimKind::Fact,
                priority: ClaimPriority::P0,
                layer: None,
                source_snapshot_id: review.source.id.clone(),
                evidence: vec![anchored.clone()],
                counterchecks: Vec::new(),
                asserted_by: ActorRef::Parent,
            },
            20,
        )
        .expect("record");
    let (readiness, verified) = store
        .verify_claim(
            SESSION,
            &review_id,
            readiness.revision,
            &recorded.id,
            verification(30),
            30,
        )
        .expect("verify");
    let ready = store
        .finalize(
            FinalizeReview {
                session_id: SESSION,
                review_id: &review_id,
                expected_revision: readiness.revision,
                requested_status: ReviewStatus::Ready,
                commit_ready: true,
                load_bearing_claim_ids: std::slice::from_ref(&verified.id),
                extra_blockers: &[],
                at_ms: 40,
            },
            |readiness, _| {
                Ok(FinalizeObservation {
                    observed: vec![ObservedClaimEvidence {
                        claim_id: verified.id.clone(),
                        evidence: Ok(vec![anchored.clone()]),
                    }],
                    current_source: readiness.source.clone(),
                    plan_current: true,
                    source_changed_during_verification: false,
                })
            },
        )
        .expect("ready");
    assert_eq!(ready.readiness.status, ReviewStatus::Ready);
    assert!(ready.readiness.receipt.is_some());

    std::fs::write(repository.path().join(".zuno/plan.md"), "second plan\n")
        .expect("mutate artifact");
    let get_tool = ReviewGetTool::new(service, plans);
    let output = get_tool
        .run(
            ReviewGetParams {
                review_id: review_id.clone(),
                claim_ids: Vec::new(),
                max_bytes: None,
            },
            ToolContext::new(
                SESSION,
                "msg_get",
                "call_get",
                "review",
                Arc::new(AllowAll),
                Arc::new(NeverInterrupted),
            ),
        )
        .await
        .expect("get review");
    let output: serde_json::Value = serde_json::from_str(&output.output).expect("get output JSON");
    assert_eq!(output["review"]["status"], "draft");
    let refreshed = store
        .review(SESSION, &review_id)
        .expect("review")
        .expect("review exists");
    assert!(refreshed.receipt.is_none());
    assert_ne!(
        refreshed
            .source
            .artifact
            .as_ref()
            .expect("artifact")
            .content_digest,
        ready
            .readiness
            .source
            .artifact
            .as_ref()
            .expect("artifact")
            .content_digest
    );
}

#[tokio::test]
async fn review_get_invalidates_ready_when_an_ignored_claim_anchor_changes() {
    let repository = tempfile::tempdir().expect("repository");
    run_git(repository.path(), &["init"]);
    run_git(
        repository.path(),
        &["config", "user.email", "test@example.com"],
    );
    run_git(repository.path(), &["config", "user.name", "Test"]);
    std::fs::create_dir_all(repository.path().join("src")).expect("src");
    std::fs::create_dir_all(repository.path().join(".zuno")).expect("ignored directory");
    std::fs::write(repository.path().join("src/lib.rs"), "pub fn run() {}\n").expect("source");
    std::fs::write(repository.path().join(".gitignore"), ".zuno/\n").expect("gitignore");
    std::fs::write(
        repository.path().join(".zuno/evidence.txt"),
        "first evidence\n",
    )
    .expect("ignored evidence");
    run_git(repository.path(), &["add", "src/lib.rs", ".gitignore"]);
    run_git(repository.path(), &["commit", "-m", "seed"]);

    let (_root, _pool, store) = fixture();
    let service = Arc::new(ReviewService::new(
        Arc::new(store.clone()),
        Arc::new(RepositoryReviewSourceProbe::new(repository.path())),
    ));
    let plans: Arc<dyn ReviewPlanProbe> = Arc::new(NoReviewPlanProbe);
    let review = service
        .open_review(
            ReviewOpenRequest {
                session_id: SESSION,
                artifact_path: None,
                scope_paths: &["src/lib.rs".to_owned()],
                plan: None,
                at_ms: 10,
            },
            plans.as_ref(),
        )
        .expect("open review");
    let review = seed_council(&store, &review);
    let anchored = service
        .source()
        .anchor(&EvidenceAnchor {
            path: ".zuno/evidence.txt".to_owned(),
            symbol: None,
            start_line: Some(1),
            end_line: Some(1),
            content_digest: None,
            reference: None,
        })
        .expect("anchor ignored evidence");
    let (readiness, recorded) = store
        .record_claim(
            SESSION,
            review.revision,
            NewReviewClaim {
                review_id: review.review_id.clone(),
                statement: "the ignored evidence records the required contract".to_owned(),
                kind: ClaimKind::Fact,
                priority: ClaimPriority::P0,
                layer: None,
                source_snapshot_id: review.source.id.clone(),
                evidence: vec![anchored.clone()],
                counterchecks: Vec::new(),
                asserted_by: ActorRef::Parent,
            },
            20,
        )
        .expect("record");
    let (readiness, verified) = store
        .verify_claim(
            SESSION,
            &review.review_id,
            readiness.revision,
            &recorded.id,
            verification(30),
            30,
        )
        .expect("verify");
    let ready = store
        .finalize(
            FinalizeReview {
                session_id: SESSION,
                review_id: &review.review_id,
                expected_revision: readiness.revision,
                requested_status: ReviewStatus::Ready,
                commit_ready: true,
                load_bearing_claim_ids: std::slice::from_ref(&verified.id),
                extra_blockers: &[],
                at_ms: 40,
            },
            |readiness, _| {
                Ok(FinalizeObservation {
                    observed: vec![ObservedClaimEvidence {
                        claim_id: verified.id.clone(),
                        evidence: Ok(vec![anchored.clone()]),
                    }],
                    current_source: readiness.source.clone(),
                    plan_current: true,
                    source_changed_during_verification: false,
                })
            },
        )
        .expect("ready");
    assert_eq!(ready.readiness.status, ReviewStatus::Ready);

    std::fs::write(
        repository.path().join(".zuno/evidence.txt"),
        "second evidence\n",
    )
    .expect("mutate ignored evidence");
    let output = ReviewGetTool::new(Arc::clone(&service), plans)
        .run(
            ReviewGetParams {
                review_id: review.review_id.clone(),
                claim_ids: vec![verified.id.clone()],
                max_bytes: None,
            },
            ToolContext::new(
                SESSION,
                "msg_get",
                "call_get",
                "review",
                Arc::new(AllowAll),
                Arc::new(NeverInterrupted),
            ),
        )
        .await
        .expect("get review");
    let output: serde_json::Value = serde_json::from_str(&output.output).expect("get output JSON");
    assert_eq!(output["review"]["status"], "draft");
    assert_eq!(output["claims"][0]["status"], "stale");
    assert!(
        store
            .review(SESSION, &review.review_id)
            .expect("review")
            .expect("review exists")
            .receipt
            .is_none()
    );
}

#[tokio::test]
async fn review_open_derives_authority_and_rejects_a_stale_plan_binding() {
    let (_root, _pool, store) = fixture();
    let service = Arc::new(ReviewService::new(
        Arc::new(store),
        Arc::new(FixedReviewSourceProbe::new(source("rsnap_fixed"))),
    ));
    let plans: Arc<dyn ReviewPlanProbe> = Arc::new(FixedPlanProbe(Some(ReviewPlanBinding {
        id: "plan_current".to_owned(),
        revision: 2,
    })));
    let tool = ReviewOpenTool::new(service, plans, Arc::new(NoopReviewCouncilRunner));
    let params = || ReviewOpenParams {
        artifact_path: None,
        scope_paths: vec!["src/lib.rs".to_owned()],
        plan_id: Some("plan_current".to_owned()),
        plan_revision: Some(1),
    };
    let denied = tool
        .run(
            params(),
            ToolContext::new(
                SESSION,
                "msg_denied",
                "call_denied",
                "build",
                Arc::new(AllowAll),
                Arc::new(NeverInterrupted),
            ),
        )
        .await
        .expect_err("another Agent cannot open a review ledger");
    assert!(matches!(denied, zuno_error::ToolError::Denied { .. }));

    let stale = tool
        .run(
            params(),
            ToolContext::new(
                SESSION,
                "msg_review",
                "call_review",
                "review",
                Arc::new(AllowAll),
                Arc::new(NeverInterrupted),
            ),
        )
        .await
        .expect_err("model-supplied stale Plan revision must be rejected");
    assert!(matches!(stale, zuno_error::ToolError::InvalidArgs { .. }));
}

#[tokio::test]
async fn review_open_automatically_runs_the_native_balanced_review_coordinator() {
    let (_root, _pool, store) = fixture();
    let service = Arc::new(ReviewService::new(
        Arc::new(store),
        Arc::new(FixedReviewSourceProbe::new(source("rsnap_auto"))),
    ));
    let runner = Arc::new(RecordingCouncilRunner::default());
    let tool = ReviewOpenTool::new(service, Arc::new(NoReviewPlanProbe), runner.clone());
    let output = tool
        .run(
            ReviewOpenParams {
                artifact_path: None,
                scope_paths: vec!["src/lib.rs".to_owned()],
                plan_id: None,
                plan_revision: None,
            },
            ToolContext::new(
                SESSION,
                "msg_review",
                "call_review",
                "review",
                Arc::new(AllowAll),
                Arc::new(NeverInterrupted),
            ),
        )
        .await
        .expect("open and run Council");
    let calls = runner.0.lock().expect("recorded Council");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].2, "review");
    assert!(calls[0].1.contains("ready to implement"));
    let output: serde_json::Value = serde_json::from_str(&output.output).expect("review output");
    assert_eq!(output["council"]["status"], "completed");
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

#[tokio::test]
async fn oversized_finalize_identifiers_fail_before_reaching_the_ledger() {
    let (_root, _pool, store) = fixture();
    let review = open(&store, source("rsnap_identifier_bounds"));
    let service = Arc::new(ReviewService::new(
        Arc::new(store),
        Arc::new(FixedReviewSourceProbe::new(review.source.clone())),
    ));
    let error = ReviewFinalizeTool::new(service, Arc::new(NoReviewPlanProbe))
        .run(
            ReviewFinalizeParams {
                review_id: review.review_id,
                expected_revision: review.revision,
                requested_status: ReviewStatus::Ready,
                load_bearing_claim_ids: vec!["x".repeat(32 * 1_024)],
                blockers: Vec::new(),
            },
            ToolContext::new(
                SESSION,
                "msg_finalize",
                "call_finalize",
                "review",
                Arc::new(AllowAll),
                Arc::new(NeverInterrupted),
            ),
        )
        .await
        .expect_err("oversized claim id");
    let rendered = zuno_error::source::describe(&error);
    assert!(rendered.chars().count() < 1_500, "{rendered}");
    assert!(rendered.contains("128"), "{rendered}");
}

#[tokio::test]
async fn review_service_is_published_by_its_native_component() {
    let (_root, _pool, store) = fixture();
    let service = Arc::new(ReviewService::new(
        Arc::new(store),
        Arc::new(FixedReviewSourceProbe::new(source("rsnap_component"))),
    ));
    let runtime = zuno_runtime::HarnessRuntime::new("review-component-test");
    let profile = zuno_runtime::HarnessProfile::new("review-component-profile")
        .with_bundle(review_bundle(Arc::clone(&service)));
    runtime
        .activate_profile(profile)
        .await
        .expect("activate review component");
    let published = runtime.service::<ReviewService>().expect("review service");
    assert!(Arc::ptr_eq(&published, &service));
    runtime.shutdown().await.expect("shutdown");
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

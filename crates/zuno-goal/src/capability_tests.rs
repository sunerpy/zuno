use super::*;
use crate::{GoalError, GoalKind, GoalStatus, GoalStore, SystemStatus};
use tempfile::TempDir;
use zuno_db::verification::{ExitAuthority, NewVerificationReceipt, ReceiptOutcome};

const SESSION: &str = "ses_capability";
const CAPABILITY: &str = "bedrock:converse:structured_output";
const SUBJECT: &str = "vendor.model-a-v1:0";
const VENDOR_DOC: &str = "https://docs.example.invalid/models/model-a#structured-output";

/// A store plus the temporary directory its spilled objectives live in.
struct Fixture {
    store: GoalStore,
    _spill: TempDir,
}

impl Fixture {
    fn in_memory() -> Self {
        let spill = tempfile::tempdir().expect("create spill directory");
        let store =
            GoalStore::open_memory(spill.path().to_path_buf()).expect("open in-memory goal store");
        Self {
            store,
            _spill: spill,
        }
    }

    /// Record a receipt the way the runtime's verifying tool path does, so every
    /// test below is about what the store does with a *real* stored receipt.
    fn record_receipt(
        &self,
        session_id: &str,
        id: &str,
        outcome: ReceiptOutcome,
        exit_authority: ExitAuthority,
        time_created: i64,
    ) {
        let connection = self.store.pool().get().expect("check out connection");
        zuno_db::verification::record(
            &connection,
            &NewVerificationReceipt {
                id: id.to_owned(),
                session_id: session_id.to_owned(),
                turn_id: Some("turn-probe".to_owned()),
                tool_call_id: format!("call-{id}"),
                tool_id: "shell".to_owned(),
                summary: "aws bedrock-runtime converse --model-id vendor.model-a-v1:0".to_owned(),
                workdir: None,
                exit_code: Some(0),
                exit_authority,
                outcome,
                git_head: None,
                output_digest: None,
                detail: None,
                time_created,
            },
        )
        .expect("record verification receipt");
    }

    fn passing_receipt(&self, id: &str, time_created: i64) {
        self.record_receipt(
            SESSION,
            id,
            ReceiptOutcome::Passed,
            ExitAuthority::Authoritative,
            time_created,
        );
    }

    /// Record the one claim these tests are about, with the given provenance.
    fn claim(
        &self,
        state: CapabilityClaimState,
        sources: &[&str],
        probe_receipt_id: Option<&str>,
        at_ms: i64,
    ) -> Result<CapabilityClaimOutcome, GoalError> {
        self.store.record_capability_claim(
            SESSION,
            &NewCapabilityClaim {
                capability: CAPABILITY.to_owned(),
                subject: SUBJECT.to_owned(),
                state,
                sources: sources.iter().map(|source| (*source).to_owned()).collect(),
                probe_receipt_id: probe_receipt_id.map(str::to_owned),
            },
            at_ms,
        )
    }

    fn claims(&self) -> Vec<CapabilityClaim> {
        self.store.capability_claims(SESSION).expect("read claims")
    }

    /// Replay the tool call that produced `original_id`, the way the runtime's
    /// `(session_id, tool_call_id)` upsert does: the row keeps the call and takes the
    /// new id and outcome, so the old id no longer resolves.
    fn replay_receipt(&self, original_id: &str, replayed_id: &str, outcome: ReceiptOutcome) {
        let connection = self.store.pool().get().expect("check out connection");
        zuno_db::verification::record(
            &connection,
            &NewVerificationReceipt {
                id: replayed_id.to_owned(),
                session_id: SESSION.to_owned(),
                turn_id: Some("turn-probe".to_owned()),
                tool_call_id: format!("call-{original_id}"),
                tool_id: "shell".to_owned(),
                summary: "aws bedrock-runtime converse --model-id vendor.model-a-v1:0".to_owned(),
                workdir: None,
                exit_code: Some(1),
                exit_authority: ExitAuthority::Authoritative,
                outcome,
                git_head: None,
                output_digest: None,
                detail: None,
                time_created: 10,
            },
        )
        .expect("replay the receipt");
    }
}

/// Block until the wall clock has moved past `after_ms`.
///
/// The store stamps a goal's creation with the wall clock, and the completion audit
/// compares claims against it. A test that records a claim under one goal and then
/// creates the next needs the second creation to be unambiguously later, which two
/// calls in the same millisecond would not be.
fn wait_for_clock_after(after_ms: i64) {
    while crate::store::now_ms().expect("read the clock") <= after_ms {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// A change goal with one criterion already proven, so only the claims decide.
fn proven_change_goal(fixture: &Fixture) -> crate::Goal {
    let created = fixture
        .store
        .create_goal_with_criteria(
            SESSION,
            "enable structured output for model a",
            &["the provider request succeeds".to_owned()],
            None,
        )
        .expect("create goal with criteria");
    let goal = created.goal;
    fixture
        .store
        .escalate_to_change(SESSION, "`write` wrote zuno.toml", goal.created_at_ms)
        .expect("escalate to a change goal");
    fixture.passing_receipt("rec_gates", goal.created_at_ms);
    fixture
        .store
        .satisfy_criterion(
            SESSION,
            goal.revision,
            "c1",
            "rec_gates",
            goal.created_at_ms,
        )
        .expect("prove the criterion")
        .goal
}

#[test]
fn states_rank_probed_over_documented_over_inferred_over_unknown() {
    use CapabilityClaimState::{Documented, Inferred, Probed, Unknown};
    assert!(Documented.is_weaker_than(Probed));
    assert!(Inferred.is_weaker_than(Documented));
    assert!(Unknown.is_weaker_than(Inferred));
    assert!(!Probed.is_weaker_than(Probed));
    assert!(!Probed.is_weaker_than(Unknown));
    assert!(Documented.may_be_relied_on() && Probed.may_be_relied_on());
    assert!(!Inferred.may_be_relied_on() && !Unknown.may_be_relied_on());
    for state in CapabilityClaimState::ALL {
        assert_eq!(
            CapabilityClaimState::parse(state.as_str()).expect("round trip"),
            state
        );
    }
    let corrupt = CapabilityClaimState::parse("guessed").expect_err("outside the CHECK set");
    assert!(matches!(
        corrupt,
        GoalError::UnknownCapabilityClaimState { .. }
    ));
    assert!(
        !corrupt.is_model_refusal(),
        "a corrupt column is not the model's to fix"
    );
}

#[test]
fn a_documented_claim_requires_at_least_one_source_that_names_something() {
    let fixture = Fixture::in_memory();

    for sources in [&[][..], &["   ", ""][..]] {
        let refusal = fixture
            .claim(CapabilityClaimState::Documented, sources, None, 1_000)
            .expect_err("a claim with no citation is not documentation");
        assert!(matches!(
            &refusal,
            GoalError::CapabilityUndocumented { capability, subject }
                if capability == CAPABILITY && subject == SUBJECT
        ));
        assert!(refusal.is_model_refusal(), "{refusal}");
        assert!(
            refusal.to_string().contains("`inferred`"),
            "the refusal names the honest alternative: {refusal}"
        );
    }
    assert!(fixture.claims().is_empty(), "a refused claim leaves no row");

    let recorded = fixture
        .claim(
            CapabilityClaimState::Documented,
            &[&format!("  {VENDOR_DOC}  "), ""],
            None,
            1_000,
        )
        .expect("a cited document is documentation");
    assert_eq!(recorded.claim.sources, [VENDOR_DOC]);
    assert_eq!(recorded.previous_state, None);
    assert!(recorded.claim.state.may_be_relied_on());
    assert_eq!(recorded.claim.time_created, 1_000);
    assert_eq!(recorded.claim.time_updated, 1_000);
}

#[test]
fn a_probed_claim_requires_a_receipt_that_proves_the_probe_was_observed_in_this_session() {
    let fixture = Fixture::in_memory();

    let uncited = fixture
        .claim(CapabilityClaimState::Probed, &[], None, 1_000)
        .expect_err("a probe nobody can cite was not observed");
    assert!(matches!(uncited, GoalError::CapabilityProbeUncited { .. }));
    assert!(uncited.is_model_refusal(), "{uncited}");

    let imagined = fixture
        .claim(
            CapabilityClaimState::Probed,
            &[],
            Some("rec_imagined"),
            1_000,
        )
        .expect_err("a receipt id the model made up proves nothing");
    assert!(matches!(
        &imagined,
        GoalError::CapabilityProbeUnproven { receipt_id, .. } if receipt_id == "rec_imagined"
    ));
    assert!(
        imagined.to_string().contains("no receipt with that id"),
        "{imagined}"
    );

    fixture.record_receipt(
        SESSION,
        "rec_failed",
        ReceiptOutcome::Failed,
        ExitAuthority::Authoritative,
        500,
    );
    let failed = fixture
        .claim(CapabilityClaimState::Probed, &[], Some("rec_failed"), 1_000)
        .expect_err("a failed probe proves the opposite");
    assert!(failed.to_string().contains("failed"), "{failed}");

    fixture.record_receipt(
        SESSION,
        "rec_derived",
        ReceiptOutcome::Passed,
        ExitAuthority::Derived,
        500,
    );
    let derived = fixture
        .claim(
            CapabilityClaimState::Probed,
            &[],
            Some("rec_derived"),
            1_000,
        )
        .expect_err("a status inferred from the last stage of a pipeline is not an observation");
    assert!(derived.to_string().contains("derived"), "{derived}");

    fixture.record_receipt(
        "ses_somebody_else",
        "rec_borrowed",
        ReceiptOutcome::Passed,
        ExitAuthority::Authoritative,
        500,
    );
    let borrowed = fixture
        .claim(
            CapabilityClaimState::Probed,
            &[],
            Some("rec_borrowed"),
            1_000,
        )
        .expect_err("a receipt is evidence about the run that produced it");
    assert!(matches!(
        borrowed,
        GoalError::CapabilityProbeUnproven { .. }
    ));

    assert!(fixture.claims().is_empty(), "no refused probe left a row");

    fixture.passing_receipt("rec_probe", 500);
    let probed = fixture
        .claim(
            CapabilityClaimState::Probed,
            &[],
            Some(" rec_probe "),
            1_000,
        )
        .expect("an observed, authoritative, passing probe is evidence");
    assert_eq!(probed.claim.state, CapabilityClaimState::Probed);
    assert_eq!(probed.claim.probe_receipt_id.as_deref(), Some("rec_probe"));
}

#[test]
fn a_probe_receipt_older_than_the_last_workspace_change_is_refused_as_stale() {
    let fixture = Fixture::in_memory();
    fixture
        .store
        .create_goal(SESSION, "enable structured output", None)
        .expect("create goal");
    fixture.passing_receipt("rec_probe", 2_000);
    fixture
        .store
        .mark_mutation(SESSION, 4_000)
        .expect("record a workspace change");

    let refusal = fixture
        .claim(CapabilityClaimState::Probed, &[], Some("rec_probe"), 5_000)
        .expect_err("a probe made before the last write describes a configuration that is gone");

    assert!(matches!(
        refusal,
        GoalError::CapabilityProbeStale {
            marked_at_ms: 4_000,
            receipt_at_ms: 2_000,
            ..
        }
    ));
    assert!(refusal.is_model_refusal(), "{refusal}");
    assert!(
        refusal
            .to_string()
            .contains("probe again after the last change"),
        "{refusal}"
    );
    assert!(fixture.claims().is_empty());
}

#[test]
fn inferred_and_unknown_claims_are_always_accepted_and_never_relied_on() {
    let fixture = Fixture::in_memory();

    let inferred = fixture
        .claim(
            CapabilityClaimState::Inferred,
            &["https://docs.example.invalid/models/model-b"],
            Some("rec_irrelevant"),
            1_000,
        )
        .expect("an honest guess is always recordable");
    assert!(!inferred.claim.state.may_be_relied_on());
    assert_eq!(
        inferred.claim.sources,
        ["https://docs.example.invalid/models/model-b"],
        "what the inference rested on is kept as context"
    );
    assert_eq!(
        inferred.claim.probe_receipt_id, None,
        "a receipt hanging off an inferred claim would read as evidence later"
    );

    let unknown = fixture
        .store
        .record_capability_claim(
            SESSION,
            &NewCapabilityClaim {
                capability: CAPABILITY.to_owned(),
                subject: "vendor.model-c-v1:0".to_owned(),
                state: CapabilityClaimState::Unknown,
                sources: Vec::new(),
                probe_receipt_id: None,
            },
            1_100,
        )
        .expect("not having looked is recordable too");
    assert!(!unknown.claim.state.may_be_relied_on());
    assert_eq!(fixture.claims().len(), 2);
}

#[test]
fn a_blank_capability_or_subject_is_refused_before_anything_is_written() {
    let fixture = Fixture::in_memory();
    for (capability, subject, field) in [("  ", SUBJECT, "capability"), (CAPABILITY, "", "subject")]
    {
        let refusal = fixture
            .store
            .record_capability_claim(
                SESSION,
                &NewCapabilityClaim {
                    capability: capability.to_owned(),
                    subject: subject.to_owned(),
                    state: CapabilityClaimState::Inferred,
                    sources: Vec::new(),
                    probe_receipt_id: None,
                },
                1_000,
            )
            .expect_err("half a sentence is not a claim");
        assert!(matches!(
            refusal,
            GoalError::EmptyCapabilityClaimField { field: refused } if refused == field
        ));
        assert!(refusal.is_model_refusal(), "{refusal}");
    }
    assert!(fixture.claims().is_empty());
}

#[test]
fn recording_the_same_capability_and_subject_again_updates_one_row_and_reports_the_retraction() {
    let fixture = Fixture::in_memory();
    let first = fixture
        .claim(CapabilityClaimState::Inferred, &[], None, 1_000)
        .expect("first recording");

    let upgraded = fixture
        .claim(CapabilityClaimState::Documented, &[VENDOR_DOC], None, 2_000)
        .expect("a document was found");
    assert_eq!(
        upgraded.previous_state,
        Some(CapabilityClaimState::Inferred)
    );
    assert!(
        !upgraded.is_retraction(),
        "documented is stronger than inferred"
    );
    assert_eq!(
        upgraded.claim.id, first.claim.id,
        "the row keeps its identity"
    );
    assert_eq!(upgraded.claim.time_created, 1_000);
    assert_eq!(upgraded.claim.time_updated, 2_000);

    let retracted = fixture
        .claim(CapabilityClaimState::Inferred, &[], None, 3_000)
        .expect("new information retracts a claim, and that is recorded rather than refused");
    assert_eq!(
        retracted.previous_state,
        Some(CapabilityClaimState::Documented)
    );
    assert!(retracted.is_retraction());
    assert_eq!(retracted.claim.state, CapabilityClaimState::Inferred);
    assert_eq!(retracted.claim.sources, Vec::<String>::new());

    let same = fixture
        .claim(CapabilityClaimState::Inferred, &[], None, 4_000)
        .expect("re-recording the same state");
    assert_eq!(same.previous_state, Some(CapabilityClaimState::Inferred));
    assert!(!same.is_retraction());

    let claims = fixture.claims();
    assert_eq!(
        claims.len(),
        1,
        "the ledger holds one row per (capability, subject)"
    );
    assert_eq!(claims[0], same.claim);

    fixture
        .store
        .record_capability_claim(
            "ses_other",
            &NewCapabilityClaim {
                capability: CAPABILITY.to_owned(),
                subject: SUBJECT.to_owned(),
                state: CapabilityClaimState::Unknown,
                sources: Vec::new(),
                probe_receipt_id: None,
            },
            5_000,
        )
        .expect("another session's claim");
    assert_eq!(
        fixture.claims().len(),
        1,
        "claims are scoped to the session that made them"
    );
}

#[test]
fn a_change_goal_cannot_complete_while_an_inferred_or_unknown_claim_stands() {
    let fixture = Fixture::in_memory();
    let goal = proven_change_goal(&fixture);
    fixture
        .claim(
            CapabilityClaimState::Inferred,
            &[],
            None,
            goal.created_at_ms + 1,
        )
        .expect("record the guess");

    let refusal = fixture
        .store
        .complete_checked(SESSION, goal.revision)
        .expect_err("a configuration written on a guess is not done");
    match &refusal {
        GoalError::CapabilityUnverified { claims } => {
            assert_eq!(claims.len(), 1);
            assert_eq!(claims[0].capability, CAPABILITY);
            assert_eq!(claims[0].subject, SUBJECT);
            assert_eq!(claims[0].state, CapabilityClaimState::Inferred);
        }
        other => panic!("expected CapabilityUnverified, got {other}"),
    }
    let message = refusal.to_string();
    assert!(
        message.contains(CAPABILITY) && message.contains(SUBJECT),
        "the refusal names the reliance to settle: {message}"
    );
    assert!(
        message.contains("only `documented` or `probed` claims may be relied on"),
        "{message}"
    );
    assert!(refusal.is_model_refusal(), "{refusal}");
    assert_eq!(
        fixture.store.goal(SESSION).expect("read goal"),
        Some(goal.clone()),
        "a refused completion leaves the goal untouched"
    );

    fixture
        .claim(
            CapabilityClaimState::Unknown,
            &[],
            None,
            goal.created_at_ms + 2,
        )
        .expect("record that it was never checked");
    assert!(matches!(
        fixture.store.complete_checked(SESSION, goal.revision),
        Err(GoalError::CapabilityUnverified { .. })
    ));

    fixture
        .claim(
            CapabilityClaimState::Documented,
            &[VENDOR_DOC],
            None,
            goal.created_at_ms + 3,
        )
        .expect("cite the vendor document for this exact subject");
    let completed = fixture
        .store
        .complete_checked(SESSION, goal.revision)
        .expect("a cited claim may be relied on")
        .expect("goal exists");
    assert_eq!(completed.status, GoalStatus::Complete);
}

#[test]
fn a_probed_claim_stops_counting_once_a_later_write_retires_its_receipt() {
    let fixture = Fixture::in_memory();
    let goal = proven_change_goal(&fixture);
    let t0 = goal.created_at_ms;
    fixture.passing_receipt("rec_probe", t0 + 1);
    let probed = fixture
        .claim(CapabilityClaimState::Probed, &[], Some("rec_probe"), t0 + 2)
        .expect("record the observed probe");
    assert!(probed.claim.state.may_be_relied_on());

    // The configuration is written after the probe, then the criterion is proven
    // again after that write, so only the probe is older than the last change.
    fixture
        .store
        .mark_mutation(SESSION, t0 + 3)
        .expect("record the write");
    fixture.passing_receipt("rec_gates_2", t0 + 4);
    let goal = fixture
        .store
        .satisfy_criterion(SESSION, goal.revision, "c1", "rec_gates_2", t0 + 5)
        .expect("prove the criterion again")
        .goal;

    let refusal = fixture
        .store
        .complete_checked(SESSION, goal.revision)
        .expect_err("a probe older than the last write no longer describes what was written");
    match &refusal {
        GoalError::CapabilityUnverified { claims } => {
            assert_eq!(claims.len(), 1);
            assert_eq!(claims[0].state, CapabilityClaimState::Probed);
            assert!(
                claims[0]
                    .reason
                    .contains("predates the workspace change recorded at")
                    && claims[0].reason.contains("probe again"),
                "the refusal says why the receipt stopped counting: {}",
                claims[0].reason
            );
        }
        other => panic!("expected CapabilityUnverified, got {other}"),
    }
    let stored = fixture.claims();
    assert_eq!(stored[0].state, CapabilityClaimState::Probed);
    assert_eq!(
        stored[0].probe_receipt_id.as_deref(),
        Some("rec_probe"),
        "the ledger is not rewritten; the audit re-checks the receipt instead"
    );

    fixture.passing_receipt("rec_probe_2", t0 + 6);
    fixture
        .claim(
            CapabilityClaimState::Probed,
            &[],
            Some("rec_probe_2"),
            t0 + 7,
        )
        .expect("probe again after the last change");
    let completed = fixture
        .store
        .complete_checked(SESSION, goal.revision)
        .expect("a probe newer than the last write counts")
        .expect("goal exists");
    assert_eq!(completed.status, GoalStatus::Complete);
}

#[test]
fn a_probed_claim_whose_receipt_was_superseded_stops_counting() {
    let fixture = Fixture::in_memory();
    let goal = proven_change_goal(&fixture);
    fixture.passing_receipt("rec_probe", goal.created_at_ms + 1);
    fixture
        .claim(
            CapabilityClaimState::Probed,
            &[],
            Some("rec_probe"),
            goal.created_at_ms + 2,
        )
        .expect("record the observed probe");

    // A replay of the same tool call rewrites the receipt row under a new id, so the
    // id the claim cites no longer resolves.
    fixture.replay_receipt("rec_probe", "rec_probe_replayed", ReceiptOutcome::Failed);

    let refusal = fixture
        .store
        .complete_checked(SESSION, goal.revision)
        .expect_err("a row saying probed is not trusted more than its receipt");
    match &refusal {
        GoalError::CapabilityUnverified { claims } => assert!(
            claims[0].reason.contains("no longer recorded"),
            "{}",
            claims[0].reason
        ),
        other => panic!("expected CapabilityUnverified, got {other}"),
    }
}

#[test]
fn a_question_goal_completes_regardless_of_unverified_claims() {
    let fixture = Fixture::in_memory();
    let goal = fixture
        .store
        .create_goal(SESSION, "does model a support structured output?", None)
        .expect("create goal");
    fixture
        .claim(
            CapabilityClaimState::Inferred,
            &[],
            None,
            goal.created_at_ms + 1,
        )
        .expect("record the guess");
    assert_eq!(
        fixture.store.kind(SESSION).expect("read kind"),
        GoalKind::Question
    );

    let completed = fixture
        .store
        .complete_checked(SESSION, goal.revision)
        .expect("nothing was written that could rest on the claim")
        .expect("goal exists");
    assert_eq!(completed.status, GoalStatus::Complete);
}

#[test]
fn claims_outlive_the_goal_that_made_them_but_gate_only_the_goal_they_were_made_under() {
    let fixture = Fixture::in_memory();
    let first = proven_change_goal(&fixture);
    fixture
        .claim(
            CapabilityClaimState::Inferred,
            &[],
            None,
            first.created_at_ms,
        )
        .expect("record the guess under the first goal");
    assert!(
        matches!(
            fixture.store.complete_checked(SESSION, first.revision),
            Err(GoalError::CapabilityUnverified { .. })
        ),
        "the goal that acted on the guess cannot complete on it"
    );

    // The user gives up on that goal and replaces it.
    fixture
        .store
        .set_status_as_system(SESSION, SystemStatus::Cancelled)
        .expect("cancel the first goal");
    wait_for_clock_after(first.created_at_ms);
    let second = fixture
        .store
        .create_goal_with_criteria(
            SESSION,
            "revert the structured output change",
            &["the reverted configuration loads".to_owned()],
            None,
        )
        .expect("replace the goal");
    assert!(second.goal.created_at_ms > first.created_at_ms);

    let claims = fixture.claims();
    assert_eq!(
        claims.len(),
        1,
        "replacement keeps the ledger: the record of what was inferred is the point"
    );
    assert_eq!(claims[0].state, CapabilityClaimState::Inferred);

    let t0 = second.goal.created_at_ms;
    fixture
        .store
        .escalate_to_change(SESSION, "`write` wrote zuno.toml", t0 + 1)
        .expect("escalate the second goal");
    fixture.passing_receipt("rec_revert", t0 + 2);
    let goal = fixture
        .store
        .satisfy_criterion(SESSION, second.goal.revision, "c1", "rec_revert", t0 + 3)
        .expect("prove the criterion")
        .goal;
    let completed = fixture
        .store
        .complete_checked(SESSION, goal.revision)
        .expect("a claim made under an earlier goal does not gate this one")
        .expect("goal exists");
    assert_eq!(completed.status, GoalStatus::Complete);
}

//! Delegated evidence-report tests.
//!
//! The old seat contract was prose nobody parsed, so a report claiming "traced the
//! call path" was indistinguishable from one naming a file and a line range. Each test
//! here pins one rule that makes that distinction machine-checkable.

use super::*;
use serde_json::{Value, json};

/// The bound `balanced-review` actually runs under, so the tests exercise the real
/// ceiling rather than an invented one.
const SEAT_OUTPUT_BYTES: usize = 16 * 1_024;

fn limits() -> ReportLimits {
    ReportLimits::for_seat(SEAT_OUTPUT_BYTES)
}

fn anchor() -> Value {
    json!({
        "path": "crates/zuno-acp/src/lib.rs",
        "symbol": "admit_and_drive_content",
        "start_line": 120,
        "end_line": 168,
        "content_digest": "sha256:abc",
        "reference": null
    })
}

fn valid() -> Value {
    json!({
        "source_snapshot_id": "rsnap_1",
        "scope_checked": ["crates/zuno-acp/src/lib.rs"],
        "claims": [{
            "statement": "session/prompt admits concurrent input through SessionInputAdmission",
            "kind": "fact",
            "priority": "p0",
            "layer": "durable_admission",
            "evidence": [anchor()],
            "counterchecks": []
        }],
        "contradictions": [],
        "unresolved": [],
        "concise_summary": "The durable admission path exists and is reached from session/prompt."
    })
}

fn parse(value: &Value) -> Result<DelegationEvidenceReport, ReportRejection> {
    DelegationEvidenceReport::parse(&value.to_string(), limits())
}

#[test]
fn a_well_formed_report_parses_with_every_field_preserved() {
    let report = parse(&valid()).expect("a well-formed report is admitted");

    assert_eq!(report.source_snapshot_id, "rsnap_1");
    assert_eq!(report.claims.len(), 1);
    assert_eq!(report.claims[0].kind, ClaimKind::Fact);
    assert_eq!(report.claims[0].priority, ClaimPriority::P0);
    assert_eq!(
        report.claims[0].layer,
        Some(SystemLayer::DurableAdmission),
        "the layer is kept, because a conclusion true at one layer routinely fails at another"
    );
    let evidence = &report.claims[0].evidence[0];
    assert_eq!(evidence.path, "crates/zuno-acp/src/lib.rs");
    assert_eq!(evidence.start_line, Some(120));
    assert_eq!(evidence.end_line, Some(168));
    assert_eq!(evidence.content_digest.as_deref(), Some("sha256:abc"));
}

/// The size check runs before decoding, so an oversized payload never builds the
/// structures it describes.
#[test]
fn an_oversized_report_is_refused_before_it_is_decoded() {
    let raw = valid().to_string();
    let tiny = ReportLimits::for_seat(32);

    let refusal = DelegationEvidenceReport::parse(&raw, tiny)
        .expect_err("a report past the seat ceiling is refused");

    let ReportRejection::TooLarge { actual, max } = refusal else {
        panic!("expected a size refusal, got {refusal:?}");
    };
    assert_eq!(actual, raw.len());
    assert_eq!(max, 32);
}

#[test]
fn leading_and_trailing_whitespace_still_counts_toward_the_seat_limit() {
    let raw = format!("{}{}{}", " ".repeat(128), valid(), " ".repeat(128));
    let rejection = DelegationEvidenceReport::parse(
        &raw,
        ReportLimits {
            max_report_bytes: raw.len() - 1,
            ..limits()
        },
    )
    .expect_err("raw bytes, including whitespace, are bounded");
    assert!(matches!(rejection, ReportRejection::TooLarge { .. }));
}

#[test]
fn a_payload_that_is_not_the_schema_is_refused_as_malformed() {
    assert!(matches!(
        DelegationEvidenceReport::parse("not json at all", limits()),
        Err(ReportRejection::Malformed { .. })
    ));

    // The old free-text contract's shape is no longer accepted, which is the point of
    // replacing it rather than allowing both.
    let legacy = json!({
        "verdict": "looks fine",
        "confidence": 0.9,
        "evidence": ["traced the call path"],
        "risks": [],
        "recommendation": "ship it"
    });
    assert!(
        matches!(parse(&legacy), Err(ReportRejection::Malformed { .. })),
        "a verdict/confidence report has no anchors and must not be admitted"
    );
}

#[test]
fn an_unexpected_field_is_refused_rather_than_silently_dropped() {
    let mut report = valid();
    report["hidden_reasoning"] = json!("a long transcript");

    assert!(matches!(
        parse(&report),
        Err(ReportRejection::Malformed { .. })
    ));
}

#[test]
fn a_report_that_echoes_no_snapshot_is_refused() {
    let mut report = valid();
    report["source_snapshot_id"] = json!("   ");

    let refusal = parse(&report).expect_err("a report must say which source it read");
    let ReportRejection::Empty { field } = refusal else {
        panic!("expected a blank-field refusal, got {refusal:?}");
    };
    assert_eq!(field, "source_snapshot_id");
}

#[test]
fn a_report_with_no_summary_is_refused() {
    let mut report = valid();
    report["concise_summary"] = json!("");

    assert!(matches!(
        parse(&report),
        Err(ReportRejection::Empty {
            field: "concise_summary"
        })
    ));
}

#[test]
fn an_oversized_summary_is_refused_by_naming_the_budget() {
    let mut report = valid();
    report["concise_summary"] = json!("x".repeat(MAX_SUMMARY_BYTES + 1));

    let refusal = parse(&report).expect_err("the summary has its own budget");
    let ReportRejection::SummaryTooLong { actual, max } = refusal else {
        panic!("expected a summary refusal, got {refusal:?}");
    };
    assert_eq!(actual, MAX_SUMMARY_BYTES + 1);
    assert_eq!(max, MAX_SUMMARY_BYTES);
}

/// A delegation that reached nothing must say so as an open question. An empty report
/// reads to a synthesizer as "no problems found".
#[test]
fn a_report_with_no_claims_is_refused() {
    let mut report = valid();
    report["claims"] = json!([]);

    assert!(matches!(parse(&report), Err(ReportRejection::NoClaims)));
}

#[test]
fn more_claims_than_one_delegation_may_carry_is_refused() {
    let mut report = valid();
    let claim = report["claims"][0].clone();
    report["claims"] = Value::Array(vec![claim; DEFAULT_MAX_CLAIMS_PER_REPORT + 1]);

    let refusal = parse(&report).expect_err("the delegation scope was too broad");
    let ReportRejection::TooManyClaims { actual, max } = refusal else {
        panic!("expected a claim-count refusal, got {refusal:?}");
    };
    assert_eq!(actual, DEFAULT_MAX_CLAIMS_PER_REPORT + 1);
    assert_eq!(max, DEFAULT_MAX_CLAIMS_PER_REPORT);
}

#[test]
fn an_oversized_statement_is_refused_by_ordinal() {
    let mut report = valid();
    report["claims"][0]["statement"] = json!("x".repeat(MAX_CLAIM_STATEMENT_CHARS + 1));

    let refusal = parse(&report).expect_err("one claim, one conclusion");
    let ReportRejection::StatementTooLong {
        ordinal,
        actual,
        max,
    } = refusal
    else {
        panic!("expected a statement refusal, got {refusal:?}");
    };
    assert_eq!(
        ordinal, 1,
        "the ordinal locates the claim the model must edit"
    );
    assert_eq!(actual, MAX_CLAIM_STATEMENT_CHARS + 1);
    assert_eq!(max, MAX_CLAIM_STATEMENT_CHARS);
}

/// The central rule: an assertion about the repository with no anchor cannot be
/// re-checked, so it is not evidence however confident it sounds.
#[test]
fn a_repository_assertion_without_an_anchor_is_refused_but_a_recommendation_is_not() {
    let mut unanchored = valid();
    unanchored["claims"][0]["evidence"] = json!([]);
    let refusal = parse(&unanchored).expect_err("an unanchored fact is refused");
    let ReportRejection::ClaimNeedsEvidence { ordinal, kind } = refusal else {
        panic!("expected an evidence refusal, got {refusal:?}");
    };
    assert_eq!(ordinal, 1);
    assert_eq!(kind, ClaimKind::Fact);

    let mut inference = valid();
    inference["claims"][0]["kind"] = json!("inference");
    inference["claims"][0]["evidence"] = json!([]);
    assert!(
        matches!(
            parse(&inference),
            Err(ReportRejection::ClaimNeedsEvidence { .. })
        ),
        "an inference is still a statement about the repository"
    );

    let mut recommendation = valid();
    recommendation["claims"][0]["kind"] = json!("recommendation");
    recommendation["claims"][0]["evidence"] = json!([]);
    parse(&recommendation).expect("a recommendation may rest on the claims it cites");
}

#[test]
fn a_claim_citing_more_anchors_than_the_cap_is_refused() {
    let mut report = valid();
    report["claims"][0]["evidence"] = Value::Array(vec![anchor(); MAX_EVIDENCE_ANCHORS + 1]);

    let refusal = parse(&report).expect_err("a claim needing more anchors is several claims");
    let ReportRejection::TooManyAnchors { actual, max, .. } = refusal else {
        panic!("expected an anchor-count refusal, got {refusal:?}");
    };
    assert_eq!(actual, MAX_EVIDENCE_ANCHORS + 1);
    assert_eq!(max, MAX_EVIDENCE_ANCHORS);
}

#[test]
fn an_anchor_without_a_path_is_refused() {
    let mut report = valid();
    report["claims"][0]["evidence"][0]["path"] = json!("  ");

    let refusal = parse(&report).expect_err("an anchor without a path cannot be re-checked");
    let ReportRejection::AnchorNeedsPath { ordinal, anchor } = refusal else {
        panic!("expected an anchor-path refusal, got {refusal:?}");
    };
    assert_eq!((ordinal, anchor), (1, 1));
}

#[test]
fn an_anchor_whose_range_runs_backwards_is_refused() {
    let mut report = valid();
    report["claims"][0]["evidence"][0]["start_line"] = json!(200);
    report["claims"][0]["evidence"][0]["end_line"] = json!(100);

    let refusal = parse(&report).expect_err("an inverted range points at nothing");
    let ReportRejection::AnchorRangeInverted { start, end, .. } = refusal else {
        panic!("expected a range refusal, got {refusal:?}");
    };
    assert_eq!((start, end), (200, 100));
}

/// A partial sweep is honest and is admitted; it is *verification* that requires all
/// five, and the durable ledger enforces that.
#[test]
fn a_negative_claim_may_report_a_partial_sweep() {
    let mut report = valid();
    report["claims"][0]["kind"] = json!("negative_fact");
    report["claims"][0]["statement"] =
        json!("a concurrent ACP prompt can only return session_busy");
    report["claims"][0]["counterchecks"] = json!([{
        "kind": "definition_lookup",
        "detail": "read begin_turn's busy guard"
    }]);

    let parsed = parse(&report).expect("a partial sweep is admitted at the wire boundary");
    assert_eq!(parsed.negative_claims().len(), 1);
    assert_eq!(parsed.claims[0].counterchecks.len(), 1);
}

#[test]
fn a_countercheck_with_no_detail_or_a_repeated_kind_is_refused() {
    let mut blank = valid();
    blank["claims"][0]["kind"] = json!("negative_fact");
    blank["claims"][0]["counterchecks"] = json!([{"kind": "caller_callee", "detail": "  "}]);
    let refusal = parse(&blank).expect_err("a countercheck with no detail records nothing");
    let ReportRejection::InvalidCounterchecks { detail, .. } = &refusal else {
        panic!("expected a countercheck refusal, got {refusal:?}");
    };
    assert!(detail.contains("records no detail"), "{detail}");

    let mut duplicated = valid();
    duplicated["claims"][0]["kind"] = json!("negative_fact");
    duplicated["claims"][0]["counterchecks"] = json!([
        {"kind": "alternate_path", "detail": "checked the durable inbox"},
        {"kind": "alternate_path", "detail": "checked it again"}
    ]);
    let refusal = parse(&duplicated).expect_err("padding one kind is not a sweep");
    let ReportRejection::InvalidCounterchecks { detail, .. } = &refusal else {
        panic!("expected a countercheck refusal, got {refusal:?}");
    };
    assert!(detail.contains("more than once"), "{detail}");
}

/// A full sweep round-trips, so the gate is a requirement rather than a wall.
#[test]
fn a_complete_sweep_round_trips_through_the_wire_format() {
    let mut report = valid();
    report["claims"][0]["kind"] = json!("negative_fact");
    report["claims"][0]["counterchecks"] = Value::Array(
        CountercheckKind::ALL
            .iter()
            .map(|kind| json!({"kind": kind.as_str(), "detail": format!("checked {kind}")}))
            .collect(),
    );

    let parsed = parse(&report).expect("a complete sweep is admitted");
    assert_eq!(
        parsed.claims[0].counterchecks.len(),
        MAX_COUNTERCHECKS_PER_CLAIM
    );
    for kind in CountercheckKind::ALL {
        assert!(
            parsed.claims[0]
                .counterchecks
                .iter()
                .any(|recorded| recorded.kind == kind),
            "{kind} survived the round trip"
        );
    }
}

#[test]
fn a_contradiction_must_name_at_least_two_sides() {
    let mut report = valid();
    report["contradictions"] = json!([{
        "statement": "the two seats disagree about admission",
        "conflicting": ["only session_busy"]
    }]);

    let refusal = parse(&report).expect_err("a contradiction is between at least two findings");
    let ReportRejection::ContradictionNeedsTwoSides { ordinal, actual } = refusal else {
        panic!("expected a contradiction refusal, got {refusal:?}");
    };
    assert_eq!((ordinal, actual), (1, 1));
}

#[test]
fn an_unresolved_question_may_omit_its_next_check() {
    let mut report = valid();
    report["unresolved"] = json!([
        {"question": "does the ACP projection report queued?", "next_check": "read projection.rs"},
        {"question": "is the TUI wording accurate?"}
    ]);

    let parsed = parse(&report).expect("an open question without a next check is still honest");
    assert_eq!(parsed.unresolved.len(), 2);
    assert_eq!(parsed.unresolved[1].next_check, None);
}

/// The instruction is written from the types, so it cannot describe a shape the parser
/// rejects, and it cannot drift from the vocabulary the durable ledger enforces.
#[test]
fn the_seat_contract_states_the_vocabulary_and_the_caps_it_enforces() {
    let contract = seat_response_contract(limits());

    for kind in ClaimKind::ALL {
        assert!(
            contract.contains(kind.as_str()),
            "the contract names `{kind}`: {contract}"
        );
    }
    for countercheck in CountercheckKind::ALL {
        assert!(
            contract.contains(countercheck.as_str()),
            "the contract names `{countercheck}`"
        );
    }
    for layer in SystemLayer::ALL {
        assert!(
            contract.contains(layer.as_str()),
            "the contract names `{layer}`"
        );
    }
    assert!(contract.contains(&MAX_CLAIM_STATEMENT_CHARS.to_string()));
    assert!(contract.contains(&MAX_EVIDENCE_ANCHORS.to_string()));
    assert!(
        contract.contains("must cite at least one anchor"),
        "the rule that produced the original defect is stated: {contract}"
    );
    assert!(
        !contract.contains("verdict") && !contract.contains("confidence"),
        "the replaced free-text contract must not survive alongside the typed one"
    );
}

/// R1: the byte ceiling has one authority, the Council preset. A contract built for a
/// different seat size states that size, so nothing can disagree with the runtime.
#[test]
fn the_seat_contract_takes_its_byte_ceiling_from_the_preset() {
    let narrow = seat_response_contract(ReportLimits::for_seat(4_096));
    let wide = seat_response_contract(ReportLimits::for_seat(SEAT_OUTPUT_BYTES));

    assert!(narrow.contains("4096"), "{narrow}");
    assert!(wide.contains(&SEAT_OUTPUT_BYTES.to_string()), "{wide}");
    assert_ne!(
        narrow, wide,
        "the ceiling is derived, not a constant pasted into prose"
    );
    assert_eq!(
        ReportLimits::for_seat(SEAT_OUTPUT_BYTES).max_report_bytes,
        SEAT_OUTPUT_BYTES,
        "no second knob may sit between the preset and the enforced bound"
    );
}

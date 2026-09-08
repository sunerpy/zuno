//! The typed evidence report a delegated reviewer returns, replacing the free-text
//! seat contract.
//!
//! # The failure this prevents
//!
//! `council_run` used to hand each seat a prose instruction asking for a JSON object
//! with `verdict`, `confidence`, `evidence`, `risks` and `recommendation`. Nothing
//! parsed it. `evidence` was an array of sentences, so a seat could write "traced the
//! call path" without naming a file, and a parent reading the synthesis could not tell
//! that claim apart from one anchored to a symbol and a line range. One seat did
//! exactly that — it read a single mutual-exclusion guard, reported that a concurrent
//! request "only returns `session_busy`", and the conclusion propagated because there
//! was nothing to check it against.
//!
//! This module makes the report a type. A claim names what it asserts, what kind of
//! assertion it is, and where in the source it holds; an anchor names a path, and
//! optionally a symbol, a line range and a content digest. A report that cannot state
//! those things is rejected at the boundary instead of being synthesized into prose
//! that reads like evidence.
//!
//! # One vocabulary, one authority
//!
//! The claim kinds, priorities, layers, counterchecks and anchors are
//! this crate's own types, not copies. The durable ledger already decides
//! what a negative claim is and what verifying one requires, so a second vocabulary
//! here would drift from the store that ultimately refuses readiness.
//!
//! # Bounds come from the preset, not from a new knob
//!
//! [`ReportLimits::for_seat`] takes the byte ceiling from the Council preset's
//! `seat_output_bytes`. There is deliberately no configuration key for it: the preset
//! already owns seat output size, and a second setting would be a value that could
//! disagree with the one the runtime actually enforces.

use crate::model::{
    ClaimKind, ClaimPriority, Countercheck, CountercheckKind, EvidenceAnchor,
    MAX_CLAIM_STATEMENT_CHARS, MAX_EVIDENCE_ANCHORS, SystemLayer,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How many claims one delegated report may carry.
///
/// Twelve is enough for one bounded review topic and small enough that a parent can
/// read every claim. A delegate with more conclusions than this was given too broad a
/// scope, and the remedy is splitting the delegation rather than raising the cap.
pub const DEFAULT_MAX_CLAIMS_PER_REPORT: usize = 12;

/// How many bytes the human-readable summary may hold.
///
/// The summary is the only free prose that reaches the parent context directly, so it
/// is bounded separately from the structured body it accompanies.
pub const MAX_SUMMARY_BYTES: usize = 2 * 1_024;

/// How many counterchecks one claim may record.
///
/// The required sweep is [`CountercheckKind::ALL`]; the cap exists so a report cannot
/// pad the same five kinds into an unbounded list.
pub const MAX_COUNTERCHECKS_PER_CLAIM: usize = CountercheckKind::ALL.len();

/// The size bounds one report is admitted under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportLimits {
    /// Total report bytes, taken from the preset's seat output ceiling.
    pub max_report_bytes: usize,
    /// How many claims the report may carry.
    pub max_claims: usize,
    /// How many bytes the summary may hold.
    pub max_summary_bytes: usize,
}

impl ReportLimits {
    /// Bounds for one Council seat, deriving the byte ceiling from the preset.
    ///
    /// `seat_output_bytes` is the preset's own field, so the report can never be
    /// admitted at a size the Council runtime would refuse to carry.
    #[must_use]
    pub const fn for_seat(seat_output_bytes: usize) -> Self {
        Self {
            max_report_bytes: seat_output_bytes,
            max_claims: DEFAULT_MAX_CLAIMS_PER_REPORT,
            max_summary_bytes: MAX_SUMMARY_BYTES,
        }
    }
}

/// One conclusion a delegate reached, with where it holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReportedClaim {
    /// What is asserted, as one sentence.
    pub statement: String,
    /// What kind of assertion it is.
    pub kind: ClaimKind,
    /// How much the review's conclusion depends on it.
    pub priority: ClaimPriority,
    /// Which layer it is about, when it is layer-specific.
    #[serde(default)]
    pub layer: Option<SystemLayer>,
    /// Where in the source it holds.
    #[serde(default)]
    pub evidence: Vec<EvidenceAnchor>,
    /// Which of the required counterchecks were completed.
    #[serde(default)]
    pub counterchecks: Vec<Countercheck>,
}

/// Two findings that cannot both be true.
///
/// Recorded by the delegate that noticed the conflict rather than resolved silently,
/// because a contradiction a report smooths over is one the parent never sees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReportedContradiction {
    /// What the conflict is.
    pub statement: String,
    /// The conflicting claim statements, as the delegate stated them.
    pub conflicting: Vec<String>,
}

/// One question the delegate could not settle from the source it read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReportedUnresolved {
    /// What is unresolved.
    pub question: String,
    /// The next check that would settle it.
    #[serde(default)]
    pub next_check: Option<String>,
}

/// The complete report one delegated reviewer returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DelegationEvidenceReport {
    /// The source snapshot the delegate was given, echoed back.
    ///
    /// Echoing it is what lets the parent refuse a report written against a different
    /// source than the review stands on.
    pub source_snapshot_id: String,
    /// What the delegate actually examined.
    #[serde(default)]
    pub scope_checked: Vec<String>,
    /// The conclusions reached.
    pub claims: Vec<ReportedClaim>,
    /// Conflicts the delegate found and did not resolve.
    #[serde(default)]
    pub contradictions: Vec<ReportedContradiction>,
    /// Questions left open.
    #[serde(default)]
    pub unresolved: Vec<ReportedUnresolved>,
    /// A short human-readable summary.
    pub concise_summary: String,
}

/// Why a delegated report was not admitted.
///
/// Every variant names the field and the rule, because the delegate is a model and a
/// rejection it cannot act on costs a whole retry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReportRejection {
    /// The payload was not one JSON object of the expected shape.
    #[error(
        "the delegated report is not one JSON object matching the evidence-report schema: \
         {detail}"
    )]
    Malformed {
        /// What the decoder objected to.
        detail: String,
    },

    /// The report was larger than the seat's output ceiling.
    #[error(
        "the delegated report is {actual} bytes, which exceeds the {max}-byte seat ceiling; \
         keep the structured body inside the bound and leave detail in the child session"
    )]
    TooLarge {
        /// How large it came out.
        actual: usize,
        /// The ceiling it had to fit inside.
        max: usize,
    },

    /// A required field held no visible character.
    #[error("the delegated report field `{field}` must contain visible text")]
    Empty {
        /// Which field was blank.
        field: &'static str,
    },

    /// The summary was longer than the prose budget.
    #[error("the delegated report summary is {actual} bytes, which exceeds the {max}-byte budget")]
    SummaryTooLong {
        /// How long it came out.
        actual: usize,
        /// The budget it had to fit inside.
        max: usize,
    },

    /// The report carried no conclusions at all.
    #[error(
        "the delegated report carries no claims; a review delegation that reached no \
         conclusion must say so as an unresolved question rather than returning an empty report"
    )]
    NoClaims,

    /// The report carried more conclusions than one delegation may.
    #[error(
        "the delegated report carries {actual} claims, which exceeds the cap of {max}; the \
         delegation scope was too broad — split it rather than raising the cap"
    )]
    TooManyClaims {
        /// How many were supplied.
        actual: usize,
        /// The cap they had to fit inside.
        max: usize,
    },

    /// One claim's statement was longer than the ledger column allows.
    #[error(
        "claim {ordinal} of the delegated report is {actual} characters, which exceeds the \
         {max}-character cap; state one conclusion per claim"
    )]
    StatementTooLong {
        /// Which claim, counted from one.
        ordinal: usize,
        /// How long it came out, in characters.
        actual: usize,
        /// The cap it had to fit inside.
        max: usize,
    },

    /// A claim asserting something about the repository cited nothing.
    #[error(
        "claim {ordinal} of the delegated report is a `{kind}` and cites no evidence anchor; \
         name the path, and where possible the symbol and line range, that shows it"
    )]
    ClaimNeedsEvidence {
        /// Which claim, counted from one.
        ordinal: usize,
        /// The kind that requires evidence.
        kind: ClaimKind,
    },

    /// A claim cited more anchors than one claim may carry.
    #[error(
        "claim {ordinal} of the delegated report cites {actual} anchors, which exceeds the cap \
         of {max}; a claim needing more anchors is several claims"
    )]
    TooManyAnchors {
        /// Which claim, counted from one.
        ordinal: usize,
        /// How many were supplied.
        actual: usize,
        /// The cap they had to fit inside.
        max: usize,
    },

    /// An anchor named no path.
    #[error(
        "anchor {anchor} of claim {ordinal} in the delegated report names no path; an anchor \
         without a path cannot be re-checked"
    )]
    AnchorNeedsPath {
        /// Which claim, counted from one.
        ordinal: usize,
        /// Which anchor within that claim, counted from one.
        anchor: usize,
    },

    /// An anchor's line range ran backwards.
    #[error(
        "anchor {anchor} of claim {ordinal} in the delegated report ends at line {end} before \
         it starts at line {start}"
    )]
    AnchorRangeInverted {
        /// Which claim, counted from one.
        ordinal: usize,
        /// Which anchor within that claim, counted from one.
        anchor: usize,
        /// The reported first line.
        start: u32,
        /// The reported last line.
        end: u32,
    },

    /// A countercheck recorded no detail, or the same kind twice.
    #[error("the counterchecks on claim {ordinal} of the delegated report are invalid: {detail}")]
    InvalidCounterchecks {
        /// Which claim, counted from one.
        ordinal: usize,
        /// What was wrong.
        detail: String,
    },

    /// A contradiction named fewer than two conflicting findings.
    #[error(
        "contradiction {ordinal} of the delegated report names {actual} conflicting findings; a \
         contradiction is between at least two"
    )]
    ContradictionNeedsTwoSides {
        /// Which contradiction, counted from one.
        ordinal: usize,
        /// How many were named.
        actual: usize,
    },
}

impl DelegationEvidenceReport {
    /// Decode and validate one delegated report against the seat's bounds.
    ///
    /// The size check runs before decoding, so an oversized payload is refused without
    /// building the structures it describes.
    ///
    /// Validation mirrors what [`crate::ReviewStore::record_claim`] will accept,
    /// so a report admitted here cannot fail at the ledger for a reason the delegate
    /// could have been told about while it still had context. It deliberately does *not*
    /// require the full countercheck sweep on a negative claim: recording a partial sweep
    /// is honest, and it is *verification* that requires all five.
    ///
    /// # Errors
    ///
    /// [`ReportRejection`], naming the field and the rule that refused.
    pub fn parse(raw: &str, limits: ReportLimits) -> Result<Self, ReportRejection> {
        if raw.len() > limits.max_report_bytes {
            return Err(ReportRejection::TooLarge {
                actual: raw.len(),
                max: limits.max_report_bytes,
            });
        }
        let report: Self =
            serde_json::from_str(raw.trim()).map_err(|source| ReportRejection::Malformed {
                detail: source.to_string(),
            })?;
        report.validate(limits)?;
        Ok(report)
    }

    /// Check one decoded report against the seat's bounds.
    ///
    /// # Errors
    ///
    /// [`ReportRejection`], naming the field and the rule that refused.
    pub fn validate(&self, limits: ReportLimits) -> Result<(), ReportRejection> {
        if self.source_snapshot_id.trim().is_empty() {
            return Err(ReportRejection::Empty {
                field: "source_snapshot_id",
            });
        }
        if self.concise_summary.trim().is_empty() {
            return Err(ReportRejection::Empty {
                field: "concise_summary",
            });
        }
        if self.concise_summary.len() > limits.max_summary_bytes {
            return Err(ReportRejection::SummaryTooLong {
                actual: self.concise_summary.len(),
                max: limits.max_summary_bytes,
            });
        }
        if self.claims.is_empty() {
            return Err(ReportRejection::NoClaims);
        }
        if self.claims.len() > limits.max_claims {
            return Err(ReportRejection::TooManyClaims {
                actual: self.claims.len(),
                max: limits.max_claims,
            });
        }
        for (index, claim) in self.claims.iter().enumerate() {
            claim.validate(index + 1)?;
        }
        for (index, contradiction) in self.contradictions.iter().enumerate() {
            if contradiction.conflicting.len() < 2 {
                return Err(ReportRejection::ContradictionNeedsTwoSides {
                    ordinal: index + 1,
                    actual: contradiction.conflicting.len(),
                });
            }
        }
        Ok(())
    }

    /// The claims whose kind makes them absences the parent must sweep before trusting.
    #[must_use]
    pub fn negative_claims(&self) -> Vec<&ReportedClaim> {
        self.claims
            .iter()
            .filter(|claim| claim.kind.requires_countercheck())
            .collect()
    }
}

impl ReportedClaim {
    fn validate(&self, ordinal: usize) -> Result<(), ReportRejection> {
        if self.statement.trim().is_empty() {
            return Err(ReportRejection::Empty {
                field: "claims[].statement",
            });
        }
        let characters = self.statement.trim().chars().count();
        if characters > MAX_CLAIM_STATEMENT_CHARS {
            return Err(ReportRejection::StatementTooLong {
                ordinal,
                actual: characters,
                max: MAX_CLAIM_STATEMENT_CHARS,
            });
        }
        if self.kind.requires_evidence() && self.evidence.is_empty() {
            return Err(ReportRejection::ClaimNeedsEvidence {
                ordinal,
                kind: self.kind,
            });
        }
        if self.evidence.len() > MAX_EVIDENCE_ANCHORS {
            return Err(ReportRejection::TooManyAnchors {
                ordinal,
                actual: self.evidence.len(),
                max: MAX_EVIDENCE_ANCHORS,
            });
        }
        for (index, anchor) in self.evidence.iter().enumerate() {
            if anchor.path.trim().is_empty() {
                return Err(ReportRejection::AnchorNeedsPath {
                    ordinal,
                    anchor: index + 1,
                });
            }
            if let (Some(start), Some(end)) = (anchor.start_line, anchor.end_line)
                && end < start
            {
                return Err(ReportRejection::AnchorRangeInverted {
                    ordinal,
                    anchor: index + 1,
                    start,
                    end,
                });
            }
        }
        self.validate_counterchecks(ordinal)
    }

    fn validate_counterchecks(&self, ordinal: usize) -> Result<(), ReportRejection> {
        if self.counterchecks.len() > MAX_COUNTERCHECKS_PER_CLAIM {
            return Err(ReportRejection::InvalidCounterchecks {
                ordinal,
                detail: format!(
                    "{} were recorded, but there are only {MAX_COUNTERCHECKS_PER_CLAIM} distinct \
                     counterchecks",
                    self.counterchecks.len()
                ),
            });
        }
        let mut seen = Vec::with_capacity(self.counterchecks.len());
        for countercheck in &self.counterchecks {
            if countercheck.detail.trim().is_empty() {
                return Err(ReportRejection::InvalidCounterchecks {
                    ordinal,
                    detail: format!(
                        "`{}` records no detail, so nothing says what was examined",
                        countercheck.kind
                    ),
                });
            }
            if seen.contains(&countercheck.kind) {
                return Err(ReportRejection::InvalidCounterchecks {
                    ordinal,
                    detail: format!("`{}` is recorded more than once", countercheck.kind),
                });
            }
            seen.push(countercheck.kind);
        }
        Ok(())
    }
}

/// The instruction one delegated reviewer receives, replacing the free-text contract.
///
/// Written from the type rather than beside it: the field names, the closed
/// vocabularies and the caps all come from the values this module and `zuno-review`
/// already enforce, so the instruction cannot describe a shape the parser rejects.
#[must_use]
pub fn seat_response_contract(limits: ReportLimits) -> String {
    format!(
        "Return exactly one JSON object and no markdown, matching this evidence-report \
         schema.\n\
         Required fields: `source_snapshot_id` (echo back the snapshot id you were given), \
         `claims` (array), `concise_summary` (string, at most {summary} bytes).\n\
         Optional fields: `scope_checked` (array of strings naming what you actually \
         examined), `contradictions` (array of objects with `statement` and `conflicting`, \
         where `conflicting` names at least two findings), `unresolved` (array of objects \
         with `question` and optional `next_check`).\n\
         Each claim is an object with `statement` (one sentence, at most {statement} \
         characters), `kind` (one of {kinds}), `priority` (one of {priorities}), optional \
         `layer` (one of {layers}), `evidence` (array, at most {anchors} entries) and \
         optional `counterchecks`.\n\
         Each evidence anchor is an object with `path` (repository-relative, required) and \
         optional `symbol`, `start_line`, `end_line`, `content_digest`, `reference`. A claim \
         whose `kind` is anything other than `recommendation` must cite at least one anchor: \
         an assertion nobody can re-check is not evidence.\n\
         A `negative_fact` — something does not exist, or only ever does one thing — must \
         record the counterchecks you completed, each an object with `kind` (one of \
         {counterchecks}) and `detail`. Reading one guard does not establish an absence, so \
         report only the sweeps you actually performed and leave the rest to `unresolved`.\n\
         Keep the whole object under {report} bytes. Do not include hidden reasoning or tool \
         transcripts; detail stays in your own session and is retrievable by claim.",
        summary = limits.max_summary_bytes,
        statement = MAX_CLAIM_STATEMENT_CHARS,
        kinds = vocabulary(&ClaimKind::ALL.map(ClaimKind::as_str)),
        priorities = vocabulary(&ClaimPriority::ALL.map(ClaimPriority::as_str)),
        layers = vocabulary(&SystemLayer::ALL.map(SystemLayer::as_str)),
        anchors = MAX_EVIDENCE_ANCHORS,
        counterchecks = vocabulary(&CountercheckKind::ALL.map(CountercheckKind::as_str)),
        report = limits.max_report_bytes,
    )
}

fn vocabulary(values: &[&str]) -> String {
    values
        .iter()
        .map(|value| format!("`{value}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod tests;

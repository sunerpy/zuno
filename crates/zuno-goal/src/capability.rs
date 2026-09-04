//! Capability claims: what a session believes an external system can do, and how it
//! knows.
//!
//! # The failure this prevents
//!
//! A session enabled a provider feature for a model because a *related* model was
//! documented to have it, then reported success. The feature was never observed on
//! the model that was configured, and nothing durable could tell afterwards that the
//! claim had been inferred rather than checked — the transcript said "supported", and
//! so did the configuration. This module gives such a claim a durable row whose
//! `state` is its provenance, so the difference between a cited document, an observed
//! probe and a guess is written down where a completion gate can read it.
//!
//! # Four states, two of which count
//!
//! [`CapabilityClaimState::Documented`] requires at least one non-empty source,
//! because a claim with no citation is not documentation.
//! [`CapabilityClaimState::Probed`] requires a receipt recorded in this session that
//! [`VerificationReceipt::proves_success`] accepts, because a probe whose response
//! nobody observed is a guess with a request attached. Those two may be relied on.
//! [`CapabilityClaimState::Inferred`] and [`CapabilityClaimState::Unknown`] are always
//! accepted — recording a guess honestly is the point — and they block: a goal that
//! changes the workspace cannot complete while one stands.
//!
//! # Scope and lifetime
//!
//! The ledger is keyed by `(session_id, capability, subject)`. Recording the same
//! claim again updates the row, and moving to a weaker state is allowed, because new
//! information can retract a claim; the retraction lands in the row's `state` and is
//! reported to the caller as [`CapabilityClaimOutcome::previous_state`], so a tool
//! result can say so.
//!
//! Claims outlive the goal they were recorded under. They are provenance, and goal
//! replacement deliberately leaves them alone where it clears criteria and marks:
//! deleting them would recreate the failure above one goal later. The completion
//! gate, though, reads only claims recorded or updated since the current goal
//! instance was created. The obligation to verify belongs to the goal that acted on
//! the claim; carrying it into a later goal — one that may exist to revert the very
//! configuration — would leave the session no honest way to finish.
//!
//! # Coherence with evidence expiry
//!
//! A probe receipt is evidence like any other, so [`GoalStore::mark_mutation`] retires
//! it: a `probed` claim whose receipt predates the last recorded workspace change
//! stops counting as probed. The check is made at audit time, by comparing the
//! receipt's timestamp with the mutation mark, rather than by rewriting rows when the
//! mark moves. That is the cheapest rule that is also correct: the row keeps saying
//! what was claimed and which receipt was cited, and the refusal says why the receipt
//! no longer counts. Recording a `probed` claim whose receipt is already older than
//! the mark is refused up front, for the reason [`GoalStore::satisfy_criterion`]
//! refuses a stale citation — accepting it would record a claim that could never be
//! relied on.
//!
//! A probe receipt recorded before the current *goal* is refused up front for the same
//! reason, and that rule is what stops goal replacement from laundering a retired probe:
//! the replacement clears the mutation mark and keeps the receipts, so re-recording the
//! old probe under the new goal would otherwise read as current. A claim recorded while
//! no goal exists is untouched — there is nothing for it to predate, and the completion
//! audit already ignores claims older than the goal.

use crate::error::GoalError;
use crate::store::{GoalStore, has_visible_character, mutation_mark, receipt_for, unproven_reason};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;
use zuno_db::verification::VerificationReceipt;
use zuno_error::DbError;

/// The table this module owns, created by [`crate::store::AUXILIARY_SCHEMA`].
pub const CAPABILITY_CLAIM_TABLE: &str = "goal_capability_claim";

/// The columns every read below selects, in one place so a row reader can rely on
/// the names.
const CLAIM_COLUMNS: &str = "id, session_id, capability, subject, state, sources, \
                             probe_receipt_id, time_created, time_updated";

/// How a session knows that a subject has a capability.
///
/// The state is provenance, not truth value: it says where the claim came from, and
/// therefore whether anything may be built on it. The two states that rest on
/// something observable — a named document, an observed request — may be relied on;
/// the other two are recorded so that the reliance is visible, and they block a
/// gated goal until replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityClaimState {
    /// A vendor document naming this exact subject was cited.
    Documented,
    /// A real request exercised the capability and its response was observed.
    Probed,
    /// Concluded from something other than this subject's own documentation or an
    /// observed probe: a sibling model's page, a family overview, a memory.
    Inferred,
    /// Not checked at all.
    Unknown,
}

impl CapabilityClaimState {
    /// Every state, in the order the column's `CHECK` constraint lists them.
    pub const ALL: [Self; 4] = [
        Self::Documented,
        Self::Probed,
        Self::Inferred,
        Self::Unknown,
    ];

    /// The stored representation, matching the column's `CHECK` members.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Documented => "documented",
            Self::Probed => "probed",
            Self::Inferred => "inferred",
            Self::Unknown => "unknown",
        }
    }

    /// Whether a claim in this state may be relied on at all.
    ///
    /// `probed` is additionally conditional on its receipt still counting; the audit
    /// in [`GoalStore::complete_checked`] re-checks that, this method does not.
    #[must_use]
    pub const fn may_be_relied_on(self) -> bool {
        matches!(self, Self::Documented | Self::Probed)
    }

    /// Whether moving from `previous` to `self` weakens a claim.
    ///
    /// A probe outranks a document because it observed the capability in the region
    /// the session actually uses, where a document describes what the vendor intended;
    /// both outrank a guess, and a guess outranks not having looked.
    #[must_use]
    pub const fn is_weaker_than(self, previous: Self) -> bool {
        self.strength() < previous.strength()
    }

    const fn strength(self) -> u8 {
        match self {
            Self::Probed => 3,
            Self::Documented => 2,
            Self::Inferred => 1,
            Self::Unknown => 0,
        }
    }

    /// Read a stored discriminator back.
    ///
    /// # Errors
    ///
    /// [`GoalError::UnknownCapabilityClaimState`] when the column holds a value
    /// outside the `CHECK` constraint, which is corruption rather than input.
    pub fn parse(value: &str) -> Result<Self, GoalError> {
        match value {
            "documented" => Ok(Self::Documented),
            "probed" => Ok(Self::Probed),
            "inferred" => Ok(Self::Inferred),
            "unknown" => Ok(Self::Unknown),
            other => Err(GoalError::UnknownCapabilityClaimState {
                value: other.to_owned(),
            }),
        }
    }
}

impl fmt::Display for CapabilityClaimState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One claim as the ledger holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityClaim {
    /// Stable row id, minted on the first recording and kept across updates.
    pub id: String,
    /// The session that made the claim. A claim is knowledge about one run.
    pub session_id: String,
    /// The capability claimed, as free text such as
    /// `bedrock:converse:structured_output`.
    pub capability: String,
    /// What the capability is claimed about, such as a model id.
    pub subject: String,
    /// How the session knows.
    pub state: CapabilityClaimState,
    /// What was cited: URLs, document titles, file paths. Never empty for a
    /// `documented` claim.
    pub sources: Vec<String>,
    /// The receipt of the probe. `Some` exactly when the state is `probed`.
    pub probe_receipt_id: Option<String>,
    /// When the claim was first recorded, in Unix milliseconds.
    pub time_created: i64,
    /// When it was last recorded, in Unix milliseconds.
    pub time_updated: i64,
}

/// A claim to record.
///
/// Carries what the caller said; [`GoalStore::record_capability_claim`] trims the
/// text and drops blank sources before judging it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCapabilityClaim {
    /// The capability claimed.
    pub capability: String,
    /// What it is claimed about.
    pub subject: String,
    /// How the caller says it knows.
    pub state: CapabilityClaimState,
    /// Citations for a `documented` claim; optional context for any other.
    pub sources: Vec<String>,
    /// The receipt of the probe, required for a `probed` claim.
    pub probe_receipt_id: Option<String>,
}

/// What one recording left behind.
///
/// Carries the state the row held before, because a retraction has to be reportable:
/// a model that moved a claim from `probed` to `inferred` should be told it retracted
/// something, not merely that a row was written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityClaimOutcome {
    /// The claim as it now stands.
    pub claim: CapabilityClaim,
    /// The state before this write, or `None` for a first recording.
    pub previous_state: Option<CapabilityClaimState>,
}

impl CapabilityClaimOutcome {
    /// Whether this write weakened a claim that stood before it.
    #[must_use]
    pub fn is_retraction(&self) -> bool {
        self.previous_state
            .is_some_and(|previous| self.claim.state.is_weaker_than(previous))
    }
}

/// One claim the completion audit refused to rely on, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnverifiedCapability {
    /// The capability that was relied on.
    pub capability: String,
    /// What it was claimed about.
    pub subject: String,
    /// The state the ledger holds for it.
    pub state: CapabilityClaimState,
    /// Why it does not count, in the words the model needs to act on.
    pub reason: String,
}

impl fmt::Display for UnverifiedCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{}` of `{}` {}",
            self.capability, self.subject, self.reason
        )
    }
}

impl GoalStore {
    /// Record what this session believes `subject` can do, and how it knows.
    ///
    /// The rules live here and not in the tool, so no caller can record a claim the
    /// ledger would not stand behind. `documented` needs at least one non-empty
    /// source. `probed` needs `probe_receipt_id` naming a receipt recorded in this
    /// session that [`VerificationReceipt::proves_success`] accepts and that is not
    /// older than the last [`Self::mark_mutation`]; the receipt is read in the same
    /// transaction as the write, exactly as [`Self::satisfy_criterion`] does. Any
    /// other state stores no receipt, whatever the caller passed: a receipt hanging
    /// off an `inferred` claim would read as evidence afterwards, which is the
    /// confusion this ledger exists to prevent. `inferred` and `unknown` are always
    /// accepted.
    ///
    /// Recording an existing `(capability, subject)` updates the row in place, keeps
    /// its id and `time_created`, and returns the state it replaced. Moving to a
    /// weaker state is allowed — new information retracts claims — and lands in the
    /// row; see [`CapabilityClaimOutcome::is_retraction`].
    ///
    /// Deliberately touches neither the goal nor its revision, and needs no goal to
    /// exist. A claim is a fact about the session's knowledge, recorded from the same
    /// hot path as a write, and completion re-reads the ledger in its own
    /// transaction.
    ///
    /// Blank is measured with [`has_visible_character`], not with `trim`, on the
    /// capability, the subject and every source. `str::trim` strips White_Space only, so
    /// a capability of `"\u{200b}"` used to be stored and then named nothing in the
    /// [`GoalError::CapabilityUnverified`] refusal that blocks completion — a blocker a
    /// human cannot read is not a blocker they can clear. Same predicate as the criterion
    /// statements, the waiver reasons and the objective, because all four are the one
    /// model-written audit surface.
    ///
    /// # Errors
    ///
    /// [`GoalError::EmptyCapabilityClaimField`] when the capability or subject holds no
    /// visible character, [`GoalError::CapabilityUndocumented`] when `documented` cites
    /// nothing that renders,
    /// [`GoalError::CapabilityProbeUncited`] when `probed` names no receipt,
    /// [`GoalError::CapabilityProbeUnproven`] when the receipt is missing from this
    /// session, failed, undecidable, or carries a derived or absent exit status,
    /// [`GoalError::CapabilityProbeStale`] when the receipt predates the last recorded
    /// change to the workspace, [`GoalError::CapabilityProbePredatesGoal`] when the
    /// receipt was recorded before the goal instance in force, and [`GoalError::Db`] on a
    /// statement failure.
    pub fn record_capability_claim(
        &self,
        session_id: &str,
        claim: &NewCapabilityClaim,
        at_ms: i64,
    ) -> Result<CapabilityClaimOutcome, GoalError> {
        let capability = claim.capability.trim();
        if !has_visible_character(capability) {
            return Err(GoalError::EmptyCapabilityClaimField {
                field: "capability",
            });
        }
        let subject = claim.subject.trim();
        if !has_visible_character(subject) {
            return Err(GoalError::EmptyCapabilityClaimField { field: "subject" });
        }
        let sources: Vec<String> = claim
            .sources
            .iter()
            .map(|source| source.trim())
            .filter(|source| has_visible_character(source))
            .map(str::to_owned)
            .collect();
        if claim.state == CapabilityClaimState::Documented && sources.is_empty() {
            return Err(GoalError::CapabilityUndocumented {
                capability: capability.to_owned(),
                subject: subject.to_owned(),
            });
        }
        let probe_receipt_id = match claim.state {
            CapabilityClaimState::Probed => Some(
                claim
                    .probe_receipt_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|receipt_id| !receipt_id.is_empty())
                    .ok_or_else(|| GoalError::CapabilityProbeUncited {
                        capability: capability.to_owned(),
                        subject: subject.to_owned(),
                    })?
                    .to_owned(),
            ),
            CapabilityClaimState::Documented
            | CapabilityClaimState::Inferred
            | CapabilityClaimState::Unknown => None,
        };
        let sources_json = serde_json::to_string(&sources).map_err(|source| {
            GoalError::Db(DbError::Query {
                source: Box::new(source),
            })
        })?;
        let id = new_claim_id();
        self.pool().try_transaction(|tx| {
            if let Some(receipt_id) = probe_receipt_id.as_deref() {
                audit_probe_receipt(tx, session_id, capability, subject, receipt_id)?;
            }
            let previous_state = tx
                .query_row(
                    &format!(
                        "SELECT state FROM {CAPABILITY_CLAIM_TABLE} \
                         WHERE session_id = ?1 AND capability = ?2 AND subject = ?3"
                    ),
                    params![session_id, capability, subject],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(zuno_db::map_error)?
                .map(|state| CapabilityClaimState::parse(&state))
                .transpose()?;
            let mut statement = tx
                .prepare(&format!(
                    "INSERT INTO {CAPABILITY_CLAIM_TABLE} \
                     (id, session_id, capability, subject, state, sources, probe_receipt_id, \
                      time_created, time_updated) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8) \
                     ON CONFLICT(session_id, capability, subject) DO UPDATE SET \
                         state = excluded.state, \
                         sources = excluded.sources, \
                         probe_receipt_id = excluded.probe_receipt_id, \
                         time_updated = excluded.time_updated \
                     RETURNING {CLAIM_COLUMNS}"
                ))
                .map_err(zuno_db::map_error)?;
            let mut rows = statement
                .query(params![
                    id,
                    session_id,
                    capability,
                    subject,
                    claim.state.as_str(),
                    sources_json,
                    probe_receipt_id,
                    at_ms
                ])
                .map_err(zuno_db::map_error)?;
            let row = rows.next().map_err(zuno_db::map_error)?.ok_or_else(|| {
                GoalError::Db(DbError::NotFound {
                    table: CAPABILITY_CLAIM_TABLE.to_owned(),
                    id: id.clone(),
                })
            })?;
            let claim = claim_from_row(row)?;
            Ok(CapabilityClaimOutcome {
                claim,
                previous_state,
            })
        })
    }

    /// Every claim this session has recorded, oldest first.
    ///
    /// Includes claims made under earlier goals: the ledger is provenance and survives
    /// goal replacement. Whether a claim gates the *current* goal is decided by the
    /// completion audit, not by this reader.
    ///
    /// # Errors
    ///
    /// [`GoalError::Db`] on a query failure and
    /// [`GoalError::UnknownCapabilityClaimState`] when a stored state is outside the
    /// `CHECK` constraint, which is corruption.
    pub fn capability_claims(&self, session_id: &str) -> Result<Vec<CapabilityClaim>, GoalError> {
        let connection = self.pool().get()?;
        claims_from(&connection, session_id)
    }
}

/// Refuse completion while the goal relies on a claim nobody verified.
///
/// Called from the store's evidence audit on a change goal and on a goal that recorded
/// success criteria whoever asked to complete it, and on a goal that recorded neither only
/// when the *run* asked. The last case is deliberate, and so is its one exemption: the
/// human's `/goal complete` has no verb that settles a claim, so refusing it over a
/// model-written row would leave a goal that can only be cancelled. See
/// `store::CompletionAuthority`. A claim is written by the session itself, so auditing one needs no
/// tool to report what it wrote — which is exactly the reporting the goal's kind
/// depends on — and a run that guessed at a capability, wrote the configuration through
/// `shell` and therefore never escalated would otherwise carry the guess out with the
/// goal. A goal with no claims settles this in one query. Only claims recorded or
/// updated since the current goal instance was created are read; the module docs say
/// why earlier claims are kept but do not gate this goal.
///
/// `inferred` and `unknown` claims are refused for what they are. A `probed` claim is
/// refused when its receipt is gone from this session, no longer proves success, predates
/// the goal instance, or predates the last recorded workspace change — the re-check that lets
/// [`GoalStore::mark_mutation`] retire probes without rewriting the ledger. Every
/// refused claim is named, so the model can see which reliance to settle rather than
/// being told only that completion was refused.
pub(crate) fn audit_capability_claims(
    tx: &Transaction<'_>,
    session_id: &str,
) -> Result<(), GoalError> {
    let Some(created_at_ms) = goal_created_at_ms(tx, session_id)? else {
        return Ok(());
    };
    let marked_at_ms = mutation_mark(tx, session_id)?;
    let mut unverified = Vec::new();
    for claim in claims_from(tx, session_id)? {
        if claim.time_updated < created_at_ms {
            continue;
        }
        if let Some(reason) =
            unverified_reason(tx, session_id, &claim, marked_at_ms, created_at_ms)?
        {
            unverified.push(UnverifiedCapability {
                capability: claim.capability,
                subject: claim.subject,
                state: claim.state,
                reason,
            });
        }
    }
    if unverified.is_empty() {
        Ok(())
    } else {
        Err(GoalError::CapabilityUnverified { claims: unverified })
    }
}

/// Why a claim cannot be relied on right now, or `None` when it can.
///
/// Phrased to follow "`capability` of `subject`", so the refusal reads as one
/// sentence per claim.
fn unverified_reason(
    tx: &Transaction<'_>,
    session_id: &str,
    claim: &CapabilityClaim,
    marked_at_ms: Option<i64>,
    goal_created_at_ms: i64,
) -> Result<Option<String>, GoalError> {
    let reason = match claim.state {
        CapabilityClaimState::Documented => None,
        CapabilityClaimState::Inferred => Some(
            "is recorded as `inferred`, so it was concluded from something other than this \
             subject's own documentation or an observed probe"
                .to_owned(),
        ),
        CapabilityClaimState::Unknown => {
            Some("is recorded as `unknown`, so it was never checked".to_owned())
        }
        CapabilityClaimState::Probed => {
            // The store never writes a `probed` row without a receipt, but a row is
            // data and the audit must not trust it more than it trusts the receipt.
            let Some(receipt_id) = claim.probe_receipt_id.as_deref() else {
                return Ok(Some(
                    "is recorded as `probed` without a probe receipt".to_owned(),
                ));
            };
            match receipt_for(tx, session_id, receipt_id)? {
                None => Some(format!(
                    "is recorded as `probed`, but receipt `{receipt_id}` is no longer recorded \
                     for this session"
                )),
                Some(receipt) if !receipt.proves_success() => Some(format!(
                    "is recorded as `probed`, but receipt `{receipt_id}` no longer proves it: {}",
                    unproven_reason(&receipt)
                )),
                // The lower bound is re-read here for the reason the receipt itself is:
                // a row is data. `record_capability_claim` refuses a probe older than the
                // goal, so post-fix no writer produces this shape, but a row a previous
                // release stored is refused at the gate rather than believed.
                Some(receipt) if receipt.time_created < goal_created_at_ms => Some(format!(
                    "is recorded as `probed`, but receipt `{receipt_id}` recorded at {} was \
                     recorded before this goal was created at {goal_created_at_ms}; probe again \
                     under the current goal and record the claim again",
                    receipt.time_created
                )),
                Some(receipt) => match marked_at_ms {
                    Some(marked_at_ms) if marked_at_ms > receipt.time_created => Some(format!(
                        "is recorded as `probed`, but receipt `{receipt_id}` recorded at {} \
                         predates the workspace change recorded at {marked_at_ms}; probe again \
                         after the last change and record the claim again",
                        receipt.time_created
                    )),
                    _ => None,
                },
            }
        }
    };
    Ok(reason)
}

/// Check the receipt a `probed` claim cites, in the words of the refusal it earns.
///
/// Four rules, mirroring [`GoalStore::satisfy_criterion`]: the receipt has to exist in
/// this session, it has to prove success with an authoritative exit status, it has to be
/// newer than the last recorded change to the workspace, and — when a goal is in force —
/// it may not predate that goal instance.
///
/// The last rule is the sibling of [`GoalError::EvidencePredatesGoal`] and closes the
/// same laundering step at the same seam. Goal replacement clears the mutation mark and
/// keeps the receipts, so a probe the previous goal was refused for would be accepted
/// again under the next one, and the completion audit would then read the re-recorded
/// claim as current. A claim recorded while no goal exists is untouched: there is nothing
/// to predate, and [`audit_capability_claims`] already ignores claims older than the
/// goal.
fn audit_probe_receipt(
    tx: &Transaction<'_>,
    session_id: &str,
    capability: &str,
    subject: &str,
    receipt_id: &str,
) -> Result<(), GoalError> {
    let receipt: VerificationReceipt =
        receipt_for(tx, session_id, receipt_id)?.ok_or_else(|| {
            GoalError::CapabilityProbeUnproven {
                capability: capability.to_owned(),
                subject: subject.to_owned(),
                receipt_id: receipt_id.to_owned(),
                reason: "no receipt with that id was recorded for this session; cite the receipt \
                     id printed by the tool result that made the probe request"
                    .to_owned(),
            }
        })?;
    if !receipt.proves_success() {
        return Err(GoalError::CapabilityProbeUnproven {
            capability: capability.to_owned(),
            subject: subject.to_owned(),
            receipt_id: receipt_id.to_owned(),
            reason: unproven_reason(&receipt),
        });
    }
    if let Some(goal_created_at_ms) = goal_created_at_ms(tx, session_id)?
        && receipt.time_created < goal_created_at_ms
    {
        return Err(GoalError::CapabilityProbePredatesGoal {
            capability: capability.to_owned(),
            subject: subject.to_owned(),
            receipt_id: receipt_id.to_owned(),
            goal_created_at_ms,
            receipt_at_ms: receipt.time_created,
        });
    }
    if let Some(marked_at_ms) = mutation_mark(tx, session_id)?
        && marked_at_ms > receipt.time_created
    {
        return Err(GoalError::CapabilityProbeStale {
            capability: capability.to_owned(),
            subject: subject.to_owned(),
            receipt_id: receipt_id.to_owned(),
            marked_at_ms,
            receipt_at_ms: receipt.time_created,
        });
    }
    Ok(())
}

/// When the goal instance in force was created, or `None` when the session has none.
fn goal_created_at_ms(tx: &Transaction<'_>, session_id: &str) -> Result<Option<i64>, GoalError> {
    tx.query_row(
        "SELECT created_at_ms FROM goal WHERE session_id = ?1",
        params![session_id],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .map_err(zuno_db::map_error)
    .map_err(GoalError::from)
}

/// Every claim for a session, oldest first, from any connection or transaction.
fn claims_from(
    connection: &Connection,
    session_id: &str,
) -> Result<Vec<CapabilityClaim>, GoalError> {
    let mut statement = connection
        .prepare(&format!(
            "SELECT {CLAIM_COLUMNS} FROM {CAPABILITY_CLAIM_TABLE} \
             WHERE session_id = ?1 ORDER BY time_created, id"
        ))
        .map_err(zuno_db::map_error)?;
    let mut rows = statement
        .query(params![session_id])
        .map_err(zuno_db::map_error)?;
    let mut claims = Vec::new();
    while let Some(row) = rows.next().map_err(zuno_db::map_error)? {
        claims.push(claim_from_row(row)?);
    }
    Ok(claims)
}

fn claim_from_row(row: &Row<'_>) -> Result<CapabilityClaim, GoalError> {
    let state: String = row.get("state").map_err(zuno_db::map_error)?;
    let sources: String = row.get("sources").map_err(zuno_db::map_error)?;
    Ok(CapabilityClaim {
        id: row.get("id").map_err(zuno_db::map_error)?,
        session_id: row.get("session_id").map_err(zuno_db::map_error)?,
        capability: row.get("capability").map_err(zuno_db::map_error)?,
        subject: row.get("subject").map_err(zuno_db::map_error)?,
        state: CapabilityClaimState::parse(&state)?,
        sources: serde_json::from_str(&sources).map_err(|source| {
            GoalError::Db(DbError::Query {
                source: Box::new(source),
            })
        })?,
        probe_receipt_id: row.get("probe_receipt_id").map_err(zuno_db::map_error)?,
        time_created: row.get("time_created").map_err(zuno_db::map_error)?,
        time_updated: row.get("time_updated").map_err(zuno_db::map_error)?,
    })
}

fn new_claim_id() -> String {
    format!("cap_{}", uuid::Uuid::new_v4().simple())
}

#[cfg(test)]
#[path = "capability_tests.rs"]
mod tests;
